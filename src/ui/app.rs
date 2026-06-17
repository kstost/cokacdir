use chrono::{DateTime, Local};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Instant, SystemTime};

use crate::config::Settings;
use crate::keybindings::Keybindings;
use crate::services::file_ops::{self, FileOperationResult, FileOperationType, ProgressMessage};
use crate::services::remote::{
    self, ConnectionStatus, RemoteContext, RemoteProfile, SftpFileEntry,
};
use crate::services::remote_transfer;
use crate::ui::file_editor::EditorState;
use crate::ui::file_info::FileInfoState;
use crate::ui::file_viewer::ViewerState;
use crate::ui::theme::DEFAULT_THEME_NAME;
use crate::utils::format::strip_unc_prefix;

/// Encode a command as base64 for safe shell execution
/// This avoids all shell escaping issues by encoding the entire command
#[cfg(unix)]
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

#[cfg(unix)]
fn encode_command_base64(command: &str) -> String {
    BASE64.encode(command.as_bytes())
}

fn spawn_process_cancel_watchdog(
    cancel_flag: Arc<AtomicBool>,
    pid: u32,
) -> (Arc<AtomicBool>, thread::JoinHandle<()>) {
    let done = Arc::new(AtomicBool::new(false));
    let done_for_thread = done.clone();
    let token = Arc::new(crate::services::claude::CancelToken::new());
    {
        let mut guard = token.child_pid.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(pid);
    }
    let handle = thread::spawn(move || {
        while !done_for_thread.load(Ordering::Relaxed) {
            if cancel_flag.load(Ordering::Relaxed) {
                token.cancel_now();
                break;
            }
            thread::sleep(std::time::Duration::from_millis(100));
        }
    });
    (done, handle)
}

/// Theme file watcher state for hot-reload
pub struct ThemeWatchState {
    /// Path to the current theme file (if external)
    pub theme_path: Option<PathBuf>,
    /// Last modification time of the theme file
    pub last_modified: Option<SystemTime>,
    /// Counter for polling interval (check every 10 ticks = ~1 second)
    pub check_counter: u8,
}

impl ThemeWatchState {
    /// Create a new watch state for the given theme name
    pub fn watch_theme(theme_name: &str) -> Self {
        let theme_path = crate::ui::theme_loader::theme_path(theme_name);
        let last_modified = theme_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok().and_then(|m| m.modified().ok()));

        Self {
            theme_path,
            last_modified,
            check_counter: 0,
        }
    }

    /// Check if the theme file has been modified.
    /// Returns true if the file was modified and should be reloaded.
    /// Only checks every 10 calls (~1 second with 100ms tick).
    pub fn check_for_changes(&mut self) -> bool {
        self.check_counter = self.check_counter.wrapping_add(1);
        if self.check_counter % 10 != 0 {
            return false;
        }

        let Some(ref path) = self.theme_path else {
            return false;
        };

        let current_modified = match std::fs::metadata(path) {
            Ok(m) => m.modified().ok(),
            Err(_) => return false,
        };

        if current_modified != self.last_modified {
            self.last_modified = current_modified;
            return true;
        }

        false
    }

    /// Update the watch state for a new theme
    pub fn update_theme(&mut self, theme_name: &str) {
        self.theme_path = crate::ui::theme_loader::theme_path(theme_name);
        self.last_modified = self
            .theme_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok().and_then(|m| m.modified().ok()));
        self.check_counter = 0;
    }
}

/// Help screen state for scrolling
pub struct HelpState {
    pub scroll_offset: usize,
    pub max_scroll: usize,
    pub visible_height: usize,
}

impl Default for HelpState {
    fn default() -> Self {
        Self {
            scroll_offset: 0,
            max_scroll: 0,
            visible_height: 0,
        }
    }
}

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
    if cfg!(windows) {
        PathBuf::from("C:\\")
    } else {
        PathBuf::from("/")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    Name,
    Type,
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
    FilePanel,
    FileViewer,
    FileEditor,
    FileInfo,
    ProcessManager,
    Help,
    AIScreen,
    SystemInfo,
    ImageViewer,
    SearchResult,
    DiffScreen,
    DiffFileView,
    GitScreen,
    DedupScreen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogType {
    Delete,
    Mkdir,
    Mkfile,
    Rename,
    Search,
    Goto,
    Tar,
    TarExcludeConfirm,
    LargeImageConfirm,
    LargeFileConfirm,
    TrueColorWarning,
    Progress,
    TarError,
    DuplicateConflict,
    Settings,
    ExtensionHandlerError,
    BinaryFileHandler,
    GitLogDiff,
    /// Remote connection dialog - enter auth info for new server
    RemoteConnect,
    /// Remote profile save prompt - ask to save after successful connect
    RemoteProfileSave,
    EncryptConfirm,
    DecryptConfirm,
    DedupConfirm,
}

/// Settings dialog state
#[derive(Debug, Clone)]
pub struct SettingsState {
    /// Available theme names (from ~/.cokacdir/themes/)
    pub themes: Vec<String>,
    /// Currently selected theme index
    pub theme_index: usize,
    /// Currently selected field row in settings dialog (0=theme, 1=diff method)
    pub selected_field: usize,
    /// Available diff compare methods
    pub diff_methods: Vec<String>,
    /// Currently selected diff method index
    pub diff_method_index: usize,
}

impl SettingsState {
    pub fn new(settings: &Settings) -> Self {
        // Scan available themes
        let mut themes = vec!["light".to_string(), "dark".to_string()];
        if let Some(themes_dir) = Settings::themes_dir() {
            if let Ok(entries) = std::fs::read_dir(&themes_dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if path.extension().map(|e| e == "json").unwrap_or(false) {
                        if let Some(stem) = path.file_stem() {
                            let name = stem.to_string_lossy().to_string();
                            if name.contains(' ') {
                                continue;
                            }
                            if !themes.contains(&name) {
                                themes.push(name);
                            }
                        }
                    }
                }
            }
        }
        themes.sort();

        // Find current theme index
        let theme_index = themes
            .iter()
            .position(|t| t == &settings.theme.name)
            .unwrap_or(0);

        let diff_methods = vec![
            "content".to_string(),
            "modified_time".to_string(),
            "content_and_time".to_string(),
        ];
        let diff_method_index = diff_methods
            .iter()
            .position(|m| m == &settings.diff_compare_method)
            .unwrap_or(0);

        Self {
            themes,
            theme_index,
            selected_field: 0,
            diff_methods,
            diff_method_index,
        }
    }

    pub fn current_theme(&self) -> &str {
        self.themes
            .get(self.theme_index)
            .map(|s| s.as_str())
            .unwrap_or(DEFAULT_THEME_NAME)
    }

    pub fn next_theme(&mut self) {
        if !self.themes.is_empty() {
            self.theme_index = (self.theme_index + 1) % self.themes.len();
        }
    }

    pub fn prev_theme(&mut self) {
        if !self.themes.is_empty() {
            self.theme_index = if self.theme_index == 0 {
                self.themes.len() - 1
            } else {
                self.theme_index - 1
            };
        }
    }

    pub fn current_diff_method(&self) -> &str {
        self.diff_methods
            .get(self.diff_method_index)
            .map(|s| s.as_str())
            .unwrap_or("content")
    }

    pub fn next_diff_method(&mut self) {
        if !self.diff_methods.is_empty() {
            self.diff_method_index = (self.diff_method_index + 1) % self.diff_methods.len();
        }
    }

    pub fn prev_diff_method(&mut self) {
        if !self.diff_methods.is_empty() {
            self.diff_method_index = if self.diff_method_index == 0 {
                self.diff_methods.len() - 1
            } else {
                self.diff_method_index - 1
            };
        }
    }
}

/// State for remote connection dialog
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RemoteField {
    Host,
    Port,
    User,
    AuthType,
    Credential, // password or key_path depending on auth_type
    Passphrase,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RemoteAuthType {
    Password,
    KeyFile,
}

#[derive(Debug, Clone)]
pub struct RemoteConnectState {
    pub selected_field: RemoteField,
    pub host: String,
    pub port: String,
    pub user: String,
    pub auth_type: RemoteAuthType,
    pub password: String,
    pub key_path: String,
    pub passphrase: String,
    pub remote_path: String,
    pub profile_name: String,
    pub error: Option<String>,
    pub cursor_pos: usize,
    /// Some(idx) when editing an existing profile via Ctrl+E
    pub editing_profile_index: Option<usize>,
}

impl RemoteConnectState {
    pub fn new() -> Self {
        Self {
            selected_field: RemoteField::Host,
            host: String::new(),
            port: "22".to_string(),
            user: String::new(),
            auth_type: RemoteAuthType::Password,
            password: String::new(),
            key_path: "~/.ssh/id_rsa".to_string(),
            passphrase: String::new(),
            remote_path: "/".to_string(),
            profile_name: String::new(),
            error: None,
            cursor_pos: 0,
            editing_profile_index: None,
        }
    }

    pub fn from_profile(profile: &remote::RemoteProfile, profile_index: usize) -> Self {
        let (auth_type, password, key_path, passphrase) = match &profile.auth {
            remote::RemoteAuth::Password { password } => (
                RemoteAuthType::Password,
                password.clone(),
                "~/.ssh/id_rsa".to_string(),
                String::new(),
            ),
            remote::RemoteAuth::KeyFile { path, passphrase } => (
                RemoteAuthType::KeyFile,
                String::new(),
                path.clone(),
                passphrase.clone().unwrap_or_default(),
            ),
        };
        Self {
            selected_field: RemoteField::Host,
            host: profile.host.clone(),
            port: profile.port.to_string(),
            user: profile.user.clone(),
            auth_type,
            password,
            key_path,
            passphrase,
            remote_path: profile.default_path.clone(),
            profile_name: profile.name.clone(),
            error: None,
            cursor_pos: 0,
            editing_profile_index: Some(profile_index),
        }
    }

    pub fn from_parsed(user: &str, host: &str, port: u16, path: &str) -> Self {
        Self {
            selected_field: if user.is_empty() {
                RemoteField::User
            } else {
                RemoteField::AuthType
            },
            host: host.to_string(),
            port: port.to_string(),
            user: user.to_string(),
            auth_type: RemoteAuthType::Password,
            password: String::new(),
            key_path: "~/.ssh/id_rsa".to_string(),
            passphrase: String::new(),
            remote_path: path.to_string(),
            profile_name: String::new(),
            error: None,
            cursor_pos: 0,
            editing_profile_index: None,
        }
    }

    pub fn is_auth_type_field(&self) -> bool {
        self.selected_field == RemoteField::AuthType
    }

    pub fn toggle_auth_type(&mut self) {
        self.auth_type = match self.auth_type {
            RemoteAuthType::Password => RemoteAuthType::KeyFile,
            RemoteAuthType::KeyFile => RemoteAuthType::Password,
        };
    }

    pub fn next_field(&self) -> RemoteField {
        match self.selected_field {
            RemoteField::Host => RemoteField::Port,
            RemoteField::Port => RemoteField::User,
            RemoteField::User => RemoteField::AuthType,
            RemoteField::AuthType => RemoteField::Credential,
            RemoteField::Credential => match self.auth_type {
                RemoteAuthType::Password => RemoteField::Host, // wrap around
                RemoteAuthType::KeyFile => RemoteField::Passphrase,
            },
            RemoteField::Passphrase => RemoteField::Host, // wrap around
        }
    }

    pub fn prev_field(&self) -> RemoteField {
        match self.selected_field {
            RemoteField::Host => match self.auth_type {
                RemoteAuthType::Password => RemoteField::Credential, // wrap around
                RemoteAuthType::KeyFile => RemoteField::Passphrase,
            },
            RemoteField::Port => RemoteField::Host,
            RemoteField::User => RemoteField::Port,
            RemoteField::AuthType => RemoteField::User,
            RemoteField::Credential => RemoteField::AuthType,
            RemoteField::Passphrase => RemoteField::Credential,
        }
    }

    pub fn active_field_mut(&mut self) -> &mut String {
        match self.selected_field {
            RemoteField::Host => &mut self.host,
            RemoteField::Port => &mut self.port,
            RemoteField::User => &mut self.user,
            RemoteField::AuthType => &mut self.password, // placeholder - handled by toggle
            RemoteField::Credential => match self.auth_type {
                RemoteAuthType::Password => &mut self.password,
                RemoteAuthType::KeyFile => &mut self.key_path,
            },
            RemoteField::Passphrase => &mut self.passphrase,
        }
    }

    pub fn active_field_value(&self) -> &str {
        match self.selected_field {
            RemoteField::Host => &self.host,
            RemoteField::Port => &self.port,
            RemoteField::User => &self.user,
            RemoteField::AuthType => match self.auth_type {
                RemoteAuthType::Password => "Password",
                RemoteAuthType::KeyFile => "Key File",
            },
            RemoteField::Credential => match self.auth_type {
                RemoteAuthType::Password => &self.password,
                RemoteAuthType::KeyFile => &self.key_path,
            },
            RemoteField::Passphrase => &self.passphrase,
        }
    }

    pub fn to_profile(&self) -> remote::RemoteProfile {
        let port: u16 = self.port.parse().unwrap_or(22);
        let auth = match self.auth_type {
            RemoteAuthType::Password => remote::RemoteAuth::Password {
                password: self.password.clone(),
            },
            RemoteAuthType::KeyFile => remote::RemoteAuth::KeyFile {
                path: self.key_path.clone(),
                passphrase: if self.passphrase.is_empty() {
                    None
                } else {
                    Some(self.passphrase.clone())
                },
            },
        };

        let name = if self.profile_name.is_empty() {
            format!("{}@{}", self.user, self.host)
        } else {
            self.profile_name.clone()
        };

        remote::RemoteProfile {
            name,
            host: self.host.clone(),
            port,
            user: self.user.clone(),
            auth,
            default_path: self.remote_path.clone(),
        }
    }
}

/// Fuzzy match: check if all characters in pattern appear in text in order
/// e.g., "thse" matches "/path/to/base" (t-h-s-e appear in sequence)
pub fn fuzzy_match(text: &str, pattern: &str) -> bool {
    let mut text_chars = text.chars().peekable();
    for pattern_char in pattern.chars() {
        loop {
            match text_chars.next() {
                Some(c) if c == pattern_char => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

/// Resolution option for duplicate file conflicts
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    Overwrite,
    Skip,
    OverwriteAll,
    SkipAll,
}

/// State for managing file conflict resolution during paste operations
#[derive(Debug, Clone)]
pub struct ConflictState {
    /// List of conflicts: (source path, destination path, display name)
    pub conflicts: Vec<(PathBuf, PathBuf, String)>,
    /// Current conflict index being resolved
    pub current_index: usize,
    /// Files that user chose to overwrite
    pub files_to_overwrite: Vec<PathBuf>,
    /// Files that user chose to skip
    pub files_to_skip: Vec<PathBuf>,
    /// Files that passed pre-conflict validation
    pub valid_files: Vec<String>,
    /// Backup of clipboard for the operation
    pub clipboard_backup: Option<Clipboard>,
    /// Whether this is a move (cut) operation
    pub is_move_operation: bool,
    /// Target directory for the operation
    pub target_path: PathBuf,
}

/// State for tar exclude confirmation dialog
#[derive(Debug, Clone)]
pub struct TarExcludeState {
    /// Archive name to create
    pub archive_name: String,
    /// Files to archive
    pub files: Vec<String>,
    /// Paths to exclude (unsafe symlinks)
    pub excluded_paths: Vec<String>,
    /// Scroll offset for viewing excluded paths
    pub scroll_offset: usize,
}

/// State for git log diff dialog
#[derive(Debug, Clone)]
pub struct GitLogDiffState {
    pub repo_path: PathBuf,
    pub project_name: String,
    pub log_entries: Vec<crate::ui::git_screen::GitLogEntry>,
    pub selected_index: usize,
    pub scroll_offset: usize,
    pub selected_commits: Vec<String>,
    pub visible_height: usize,
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
    /// Remote profile of the source panel (None if local)
    pub source_remote_profile: Option<remote::RemoteProfile>,
}

/// File operation progress state for progress dialog
pub struct FileOperationProgress {
    pub operation_type: FileOperationType,
    pub is_active: bool,
    pub cancel_flag: Arc<AtomicBool>,
    pub receiver: Option<Receiver<ProgressMessage>>,

    // Preparation state
    pub is_preparing: bool,
    pub preparing_message: String,

    // Progress state
    pub current_file: String,
    pub current_file_progress: f64, // 0.0 ~ 1.0
    pub total_files: usize,
    pub completed_files: usize,
    pub total_bytes: u64,
    pub completed_bytes: u64,

    pub result: Option<FileOperationResult>,

    // Store last error before result is created
    last_error: Option<String>,

    // Timestamp when the operation started (for display delay)
    pub started_at: Instant,
}

impl FileOperationProgress {
    const CANCELLED_ERROR: &'static str = "Cancelled";
    const MISSING_COMPLETION_ERROR: &'static str =
        "Operation worker exited without a completion message";

    pub fn new(operation_type: FileOperationType) -> Self {
        Self {
            operation_type,
            is_active: false,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            receiver: None,
            is_preparing: false,
            preparing_message: String::new(),
            current_file: String::new(),
            current_file_progress: 0.0,
            total_files: 0,
            completed_files: 0,
            total_bytes: 0,
            completed_bytes: 0,
            result: None,
            last_error: None,
            started_at: Instant::now(),
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
                            ProgressMessage::Preparing(message) => {
                                self.is_preparing = true;
                                self.preparing_message = message;
                            }
                            ProgressMessage::PrepareComplete => {
                                self.is_preparing = false;
                                self.preparing_message.clear();
                            }
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
                            ProgressMessage::TotalProgress(
                                completed_files,
                                total_files,
                                completed_bytes,
                                total_bytes,
                            ) => {
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
                        let last_error = if self.cancel_flag.load(Ordering::Relaxed) {
                            Some(Self::CANCELLED_ERROR.to_string())
                        } else {
                            self.last_error
                                .take()
                                .or_else(|| Some(Self::MISSING_COMPLETION_ERROR.to_string()))
                        };
                        self.result = Some(FileOperationResult {
                            success_count: 0,
                            failure_count: 1,
                            last_error,
                        });
                        self.is_active = false;
                        return false;
                    }
                }
            }
        }

        self.is_active
    }

    /// Get overall progress as percentage (0.0 ~ 1.0)
    /// Incorporates partial progress of the currently transferring file
    pub fn overall_progress(&self) -> f64 {
        if self.total_bytes > 0 {
            self.completed_bytes as f64 / self.total_bytes as f64
        } else if self.total_files > 0 {
            (self.completed_files as f64 + self.current_file_progress) / self.total_files as f64
        } else {
            0.0
        }
    }
}

/// What to do after a remote file download completes
pub enum PendingRemoteOpen {
    /// Open in editor (with remote upload on save)
    Editor {
        tmp_path: PathBuf,
        panel_index: usize,
        remote_path: String,
    },
    /// Open in image viewer
    ImageViewer { tmp_path: PathBuf },
}

#[derive(Debug, Clone, Default)]
pub struct PathCompletion {
    pub suggestions: Vec<String>, // 자동완성 후보 목록
    pub selected_index: usize,    // 선택된 후보 인덱스
    pub visible: bool,            // 목록 표시 여부
}

#[derive(Debug, Clone)]
pub struct Dialog {
    pub dialog_type: DialogType,
    pub input: String,
    pub cursor_pos: usize, // 커서 위치 (문자 인덱스)
    pub message: String,
    pub completion: Option<PathCompletion>, // 경로 자동완성용
    pub selected_button: usize,             // 버튼 선택 인덱스 (0: Yes, 1: No)
    pub selection: Option<(usize, usize)>,  // 선택 범위 (start, end) - None이면 선택 없음
    pub use_md5: bool,                      // MD5 검증 옵션 (EncryptConfirm에서 사용)
}

#[derive(Debug, Clone)]
pub struct FileItem {
    pub name: String,
    /// Original filename read from .cokacenc header (plaintext, no decryption needed)
    pub display_name: Option<String>,
    pub is_directory: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified: DateTime<Local>,
    #[allow(dead_code)]
    pub permissions: String,
}

/// Parse sort_by string from settings to SortBy enum
pub fn parse_sort_by(s: &str) -> SortBy {
    match s.to_lowercase().as_str() {
        "type" => SortBy::Type,
        "size" => SortBy::Size,
        "modified" | "date" => SortBy::Modified,
        _ => SortBy::Name,
    }
}

/// Parse sort_order string from settings to SortOrder enum
pub fn parse_sort_order(s: &str) -> SortOrder {
    match s.to_lowercase().as_str() {
        "desc" => SortOrder::Desc,
        _ => SortOrder::Asc,
    }
}

/// Convert SortBy enum to string for settings
pub fn sort_by_to_string(sort_by: SortBy) -> String {
    match sort_by {
        SortBy::Name => "name".to_string(),
        SortBy::Type => "type".to_string(),
        SortBy::Size => "size".to_string(),
        SortBy::Modified => "modified".to_string(),
    }
}

/// Convert SortOrder enum to string for settings
pub fn sort_order_to_string(sort_order: SortOrder) -> String {
    match sort_order {
        SortOrder::Asc => "asc".to_string(),
        SortOrder::Desc => "desc".to_string(),
    }
}

/// Remote operation spinner — shows a spinning indicator while a remote operation runs in background
pub struct RemoteSpinner {
    pub message: String,
    pub started_at: Instant,
    pub receiver: Receiver<RemoteSpinnerResult>,
}

/// Result from a background remote operation
pub enum RemoteSpinnerResult {
    /// Operation on an existing connection (ctx returned)
    PanelOp {
        ctx: Box<RemoteContext>,
        panel_idx: usize,
        outcome: PanelOpOutcome,
    },
    /// New connection completed
    Connected {
        result: Result<ConnectSuccess, String>,
        panel_idx: usize,
    },
    /// Local background operation completed (no remote ctx)
    LocalOp {
        message: Result<String, String>,
        reload: bool,
    },
    /// Search completed
    SearchComplete {
        results: Vec<crate::ui::search_result::SearchResultItem>,
        search_term: String,
        base_path: PathBuf,
    },
    /// Git log diff preparation completed
    GitDiffComplete {
        result: Result<(PathBuf, PathBuf), String>,
    },
}

/// Outcome variants for panel operations
pub enum PanelOpOutcome {
    /// mkdir, mkfile, rename, remove, upload → reload needed
    Simple {
        message: Result<String, String>,
        pending_focus: Option<String>,
        reload: bool,
    },
    /// Remote editor save upload result. The generation prevents stale uploads
    /// from clearing a newer local save that still needs uploading.
    RemoteSave {
        message: Result<String, String>,
        remote_path: String,
        generation: u64,
        reload: bool,
    },
    /// list_dir result
    ListDir {
        entries: Result<Vec<SftpFileEntry>, String>,
        path: PathBuf,
        /// Previous path for rollback on failure (None = refresh, no rollback needed)
        old_path: Option<PathBuf>,
    },
    /// dir_exists result
    DirExists { exists: bool, target_entry: String },
}

/// Successful connection data
pub struct ConnectSuccess {
    pub ctx: Box<RemoteContext>,
    pub entries: Vec<SftpFileEntry>,
    pub path: String,
    pub fallback_msg: Option<String>,
    pub profile: RemoteProfile,
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
    pub disk_total: u64,
    pub disk_available: u64,
    /// Remote context — None means local panel
    pub remote_ctx: Option<Box<RemoteContext>>,
    /// Cached remote display info (user, host, port) — survives while remote_ctx is temporarily taken
    pub remote_display: Option<(String, String, u16)>,
}

impl PanelState {
    pub fn new(path: PathBuf) -> Self {
        // Validate path and get a valid one
        let fallback = dirs::home_dir().unwrap_or_else(|| {
            if cfg!(windows) {
                PathBuf::from("C:\\")
            } else {
                PathBuf::from("/")
            }
        });
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
            disk_total: 0,
            disk_available: 0,
            remote_ctx: None,
            remote_display: None,
        };
        state.load_files();
        state
    }

    /// Create a PanelState with settings from config
    pub fn with_settings(path: PathBuf, panel_settings: &crate::config::PanelSettings) -> Self {
        // Validate path and get a valid one
        let fallback = dirs::home_dir().unwrap_or_else(|| {
            if cfg!(windows) {
                PathBuf::from("C:\\")
            } else {
                PathBuf::from("/")
            }
        });
        let valid_path = get_valid_path(&path, &fallback);

        let sort_by = parse_sort_by(&panel_settings.sort_by);
        let sort_order = parse_sort_order(&panel_settings.sort_order);

        let mut state = Self {
            path: valid_path,
            files: Vec::new(),
            selected_index: 0,
            selected_files: HashSet::new(),
            sort_by,
            sort_order,
            scroll_offset: 0,
            pending_focus: None,
            disk_total: 0,
            disk_available: 0,
            remote_ctx: None,
            remote_display: None,
        };
        state.load_files();
        state
    }

    /// Check if this panel is connected to a remote server
    pub fn is_remote(&self) -> bool {
        self.remote_ctx.is_some() || self.remote_display.is_some()
    }

    /// Get the remote display path (user@host:/path) or local path string
    pub fn display_path(&self) -> String {
        if let Some(ref ctx) = self.remote_ctx {
            remote::format_remote_display(&ctx.profile, &self.path.display().to_string())
        } else if let Some((ref user, ref host, port)) = self.remote_display {
            let path = self.path.display().to_string();
            if port != 22 {
                format!("{}@{}:{}:{}", user, host, port, path)
            } else {
                format!("{}@{}:{}", user, host, path)
            }
        } else {
            self.path.display().to_string()
        }
    }

    pub fn load_files(&mut self) {
        if self.is_remote() {
            self.load_files_remote();
        } else {
            self.load_files_local();
        }
    }

    fn load_files_local(&mut self) {
        self.files.clear();

        // Add parent directory entry if not at root
        if self.path.parent().is_some() {
            self.files.push(FileItem {
                name: "..".to_string(),
                display_name: None,
                is_directory: true,
                is_symlink: false,
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
                let path = entry.path();

                // Check if it's a symlink first
                let symlink_meta = fs::symlink_metadata(&path).ok()?;
                let is_symlink = symlink_meta.is_symlink();

                // For symlinks, follow to get target type; for others, use direct metadata
                let metadata = if is_symlink {
                    fs::metadata(&path).ok().unwrap_or(symlink_meta.clone())
                } else {
                    symlink_meta.clone()
                };

                let is_directory = metadata.is_dir();
                let size = if is_directory { 0 } else { metadata.len() };
                let modified = metadata
                    .modified()
                    .ok()
                    .map(DateTime::<Local>::from)
                    .unwrap_or_else(Local::now);

                #[cfg(unix)]
                let permissions = {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = symlink_meta.permissions().mode();
                    crate::utils::format::format_permissions_short(mode)
                };
                #[cfg(not(unix))]
                let permissions = String::new();

                let display_name = if !is_directory && name.ends_with(crate::enc::naming::EXT) {
                    std::fs::File::open(&path)
                        .ok()
                        .and_then(|f| {
                            let mut reader = std::io::BufReader::new(f);
                            crate::enc::crypto::read_header(&mut reader).ok()
                        })
                        .and_then(|(_, _, hdr_name)| {
                            if hdr_name.is_empty() {
                                None
                            } else {
                                Some(hdr_name)
                            }
                        })
                } else {
                    None
                };

                Some(FileItem {
                    name,
                    display_name,
                    is_directory,
                    is_symlink,
                    size,
                    modified,
                    permissions,
                })
            }));

            self.sort_items(&mut items);
            self.files.reserve(items.len());
            self.files.extend(items);
        }

        self.finalize_load();
        self.update_disk_info();
    }

    fn load_files_remote(&mut self) {
        self.files.clear();

        let remote_path = self.path.display().to_string();

        // Always add parent directory entry for remote paths
        if remote_path != "/" {
            self.files.push(FileItem {
                name: "..".to_string(),
                display_name: None,
                is_directory: true,
                is_symlink: false,
                size: 0,
                modified: Local::now(),
                permissions: String::new(),
            });
        }

        let entries = if let Some(ref ctx) = self.remote_ctx {
            ctx.session.list_dir(&remote_path)
        } else {
            return;
        };

        match entries {
            Ok(sftp_entries) => {
                let mut items: Vec<FileItem> = sftp_entries
                    .into_iter()
                    .map(|entry| FileItem {
                        name: entry.name,
                        display_name: None,
                        is_directory: entry.is_directory,
                        is_symlink: entry.is_symlink,
                        size: if entry.is_directory { 0 } else { entry.size },
                        modified: entry.modified,
                        permissions: entry.permissions,
                    })
                    .collect();

                self.sort_items(&mut items);
                self.files.reserve(items.len());
                self.files.extend(items);

                // Update connection status
                if let Some(ref mut ctx) = self.remote_ctx {
                    ctx.status = ConnectionStatus::Connected;
                }
            }
            Err(e) => {
                if let Some(ref mut ctx) = self.remote_ctx {
                    ctx.status = ConnectionStatus::Disconnected(e);
                }
            }
        }

        self.finalize_load();
        // No disk info for remote panels
        self.disk_total = 0;
        self.disk_available = 0;
    }

    /// Apply remote directory listing results (no network call)
    pub fn apply_remote_entries(&mut self, entries: Vec<SftpFileEntry>, path: &Path) {
        self.files.clear();
        self.path = path.to_path_buf();

        let remote_path = path.display().to_string();
        // Always add parent directory entry for remote paths
        if remote_path != "/" {
            self.files.push(FileItem {
                name: "..".to_string(),
                display_name: None,
                is_directory: true,
                is_symlink: false,
                size: 0,
                modified: Local::now(),
                permissions: String::new(),
            });
        }

        let mut items: Vec<FileItem> = entries
            .into_iter()
            .map(|entry| FileItem {
                name: entry.name,
                display_name: None,
                is_directory: entry.is_directory,
                is_symlink: entry.is_symlink,
                size: if entry.is_directory { 0 } else { entry.size },
                modified: entry.modified,
                permissions: entry.permissions,
            })
            .collect();

        self.sort_items(&mut items);
        self.files.reserve(items.len());
        self.files.extend(items);

        self.finalize_load();
        self.disk_total = 0;
        self.disk_available = 0;
    }

    /// Sort file items (shared between local and remote)
    fn sort_items(&self, items: &mut Vec<FileItem>) {
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
                SortBy::Type => {
                    let ext_a = std::path::Path::new(&a.name)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    let ext_b = std::path::Path::new(&b.name)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    ext_a.cmp(&ext_b)
                }
                SortBy::Size => a.size.cmp(&b.size),
                SortBy::Modified => a.modified.cmp(&b.modified),
            };

            match self.sort_order {
                SortOrder::Asc => cmp,
                SortOrder::Desc => cmp.reverse(),
            }
        });
    }

    /// Finalize file loading (handle focus and bounds)
    fn finalize_load(&mut self) {
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

    fn update_disk_info(&mut self) {
        if self.is_remote() {
            self.disk_total = 0;
            self.disk_available = 0;
            return;
        }

        #[cfg(unix)]
        {
            use std::ffi::CString;
            use std::mem::MaybeUninit;

            if let Some(path_str) = self.path.to_str() {
                if let Ok(c_path) = CString::new(path_str) {
                    let mut stat: MaybeUninit<libc::statvfs> = MaybeUninit::uninit();
                    // SAFETY: statvfs is a standard POSIX function, c_path is valid
                    let result = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
                    if result == 0 {
                        // SAFETY: statvfs succeeded, stat is initialized
                        let stat = unsafe { stat.assume_init() };
                        self.disk_total = stat.f_blocks as u64 * stat.f_frsize as u64;
                        self.disk_available = stat.f_bavail as u64 * stat.f_frsize as u64;
                        return;
                    }
                }
            }
        }
        #[cfg(windows)]
        {
            // Extract drive letter from path (e.g. "C:\")
            if let Some(path_str) = self.path.to_str() {
                if path_str.len() >= 2 && path_str.as_bytes()[1] == b':' {
                    if let Ok(output) = std::process::Command::new("wmic")
                        .args([
                            "logicaldisk",
                            "where",
                            &format!("DeviceID='{}'", &path_str[..2]),
                            "get",
                            "Size,FreeSpace",
                            "/value",
                        ])
                        .output()
                    {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        for line in stdout.lines() {
                            if let Some(val) = line.strip_prefix("Size=") {
                                self.disk_total = val.trim().parse::<u64>().unwrap_or(0);
                            } else if let Some(val) = line.strip_prefix("FreeSpace=") {
                                self.disk_available = val.trim().parse::<u64>().unwrap_or(0);
                            }
                        }
                        return;
                    }
                }
            }
        }
        self.disk_total = 0;
        self.disk_available = 0;
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
        if self.is_remote() {
            // Re-sort existing items locally (no network call)
            let mut items: Vec<FileItem> =
                self.files.drain(..).filter(|f| f.name != "..").collect();
            // Re-add ".." entry
            let remote_path = self.path.display().to_string();
            if remote_path != "/" {
                self.files.push(FileItem {
                    name: "..".to_string(),
                    display_name: None,
                    is_directory: true,
                    is_symlink: false,
                    size: 0,
                    modified: Local::now(),
                    permissions: String::new(),
                });
            }
            self.sort_items(&mut items);
            self.files.reserve(items.len());
            self.files.extend(items);
            self.finalize_load();
        } else {
            self.load_files();
        }
    }
}

pub struct App {
    pub panels: Vec<PanelState>,
    pub active_panel_index: usize,
    pub current_screen: Screen,
    pub dialog: Option<Dialog>,
    pub message: Option<String>,
    pub message_timer: u8,

    // Flag to request full screen redraw (after terminal mode command)
    pub needs_full_redraw: bool,

    // Settings
    pub settings: Settings,

    // Theme (loaded from settings)
    pub theme: crate::ui::theme::Theme,

    // Theme hot-reload watcher (only active in design mode)
    pub theme_watch_state: ThemeWatchState,

    // Design mode flag (--design): enables theme hot-reload
    pub design_mode: bool,

    // Keybindings (built from settings)
    pub keybindings: Keybindings,

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
    pub ai_panel_index: Option<usize>,    // AI가 표시될 패널 인덱스
    pub ai_previous_panel: Option<usize>, // AI 화면 띄우기 전 포커스 인덱스

    // System info state
    pub system_info_state: crate::ui::system_info::SystemInfoState,

    // Advanced search state
    pub advanced_search_state: crate::ui::advanced_search::AdvancedSearchState,

    // Image viewer state
    pub image_viewer_state: Option<crate::ui::image_viewer::ImageViewerState>,

    // Image protocol picker (for inline image rendering: Kitty/iTerm2/Sixel)
    pub image_picker: Option<ratatui_image::picker::Picker>,

    // Pending large image path (for confirmation dialog)
    pub pending_large_image: Option<std::path::PathBuf>,

    // Pending large file path (for confirmation dialog)
    pub pending_large_file: Option<std::path::PathBuf>,

    // Pending binary file path and extension (for handler setup dialog)
    pub pending_binary_file: Option<(std::path::PathBuf, String)>,

    // Search result state (재귀 검색 결과)
    pub search_result_state: crate::ui::search_result::SearchResultState,

    // Track previous screen for back navigation
    pub previous_screen: Option<Screen>,

    // Clipboard state for Ctrl+C/X/V operations
    pub clipboard: Option<Clipboard>,

    // File operation progress state
    pub file_operation_progress: Option<FileOperationProgress>,

    // Pending tar archive name (for focusing after completion)
    pub pending_tar_archive: Option<String>,

    // Pending extract directory name (for focusing after completion)
    pub pending_extract_dir: Option<String>,

    // Pending paste focus names (for focusing on first pasted file after completion)
    pub pending_paste_focus: Option<Vec<String>>,

    // Conflict resolution state for duplicate file handling
    pub conflict_state: Option<ConflictState>,

    // Tar exclude confirmation state
    pub tar_exclude_state: Option<TarExcludeState>,

    // Help screen state
    pub help_state: HelpState,

    // Settings dialog state
    pub settings_state: Option<SettingsState>,

    // Remote connection dialog state
    pub remote_connect_state: Option<RemoteConnectState>,

    // Diff screen state
    pub diff_first_panel: Option<usize>,
    pub diff_state: Option<crate::ui::diff_screen::DiffState>,
    pub diff_file_view_state: Option<crate::ui::diff_file_view::DiffFileViewState>,

    // Git screen state
    pub git_screen_state: Option<crate::ui::git_screen::GitScreenState>,

    // Dedup screen state
    pub dedup_screen_state: Option<crate::ui::dedup_screen::DedupScreenState>,

    // Git log diff state
    pub git_log_diff_state: Option<GitLogDiffState>,

    // Pending remote download → open action
    pub pending_remote_open: Option<PendingRemoteOpen>,

    // Remote operation spinner (SSH/SFTP background task)
    pub remote_spinner: Option<RemoteSpinner>,
}

impl App {
    pub fn new(first_path: PathBuf, second_path: PathBuf) -> Self {
        Self {
            panels: vec![PanelState::new(first_path), PanelState::new(second_path)],
            active_panel_index: 0,
            current_screen: Screen::FilePanel,
            dialog: None,
            message: None,
            message_timer: 0,
            needs_full_redraw: false,
            settings: Settings::default(),
            theme: crate::ui::theme::Theme::default(),
            theme_watch_state: ThemeWatchState::watch_theme(DEFAULT_THEME_NAME),
            design_mode: false,
            keybindings: Keybindings::from_config(&crate::keybindings::KeybindingsConfig::default()),

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
            ai_panel_index: None,
            ai_previous_panel: None,
            system_info_state: crate::ui::system_info::SystemInfoState::default(),
            advanced_search_state: crate::ui::advanced_search::AdvancedSearchState::default(),
            image_viewer_state: None,
            image_picker: None,
            pending_large_image: None,
            pending_large_file: None,
            pending_binary_file: None,
            search_result_state: crate::ui::search_result::SearchResultState::default(),
            previous_screen: None,
            clipboard: None,
            file_operation_progress: None,
            pending_tar_archive: None,
            pending_extract_dir: None,
            pending_paste_focus: None,
            conflict_state: None,
            tar_exclude_state: None,
            help_state: HelpState::default(),
            settings_state: None,
            remote_connect_state: None,
            diff_first_panel: None,
            diff_state: None,
            diff_file_view_state: None,
            git_screen_state: None,
            dedup_screen_state: None,
            git_log_diff_state: None,
            pending_remote_open: None,
            remote_spinner: None,
        }
    }

    /// Create App with settings loaded from config file
    pub fn with_settings(settings: Settings) -> Self {
        // Build panels from settings
        let panels: Vec<PanelState> = if settings.panels.is_empty() {
            // No panels configured, create defaults
            let first = std::env::current_dir().unwrap_or_else(|_| {
                if cfg!(windows) {
                    PathBuf::from("C:\\")
                } else {
                    PathBuf::from("/")
                }
            });
            let second = dirs::home_dir().unwrap_or_else(|| {
                if cfg!(windows) {
                    PathBuf::from("C:\\")
                } else {
                    PathBuf::from("/")
                }
            });
            vec![PanelState::new(first), PanelState::new(second)]
        } else {
            settings
                .panels
                .iter()
                .map(|ps| {
                    let path = settings.resolve_path(&ps.start_path, || {
                        std::env::current_dir().unwrap_or_else(|_| {
                            if cfg!(windows) {
                                PathBuf::from("C:\\")
                            } else {
                                PathBuf::from("/")
                            }
                        })
                    });
                    PanelState::with_settings(path, ps)
                })
                .collect()
        };
        let active_panel_index = settings
            .active_panel_index
            .min(panels.len().saturating_sub(1));

        // Load theme from settings
        let theme = crate::ui::theme::Theme::load(&settings.theme.name);
        let theme_watch_state = ThemeWatchState::watch_theme(&settings.theme.name);

        // Build keybindings from settings
        let keybindings = Keybindings::from_config(&settings.keybindings);

        Self {
            panels,
            active_panel_index,
            current_screen: Screen::FilePanel,
            dialog: None,
            message: None,
            message_timer: 0,
            needs_full_redraw: false,
            settings,
            theme,
            theme_watch_state,
            design_mode: false,
            keybindings,

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
            ai_panel_index: None,
            ai_previous_panel: None,
            system_info_state: crate::ui::system_info::SystemInfoState::default(),
            advanced_search_state: crate::ui::advanced_search::AdvancedSearchState::default(),
            image_viewer_state: None,
            image_picker: None,
            pending_large_image: None,
            pending_large_file: None,
            pending_binary_file: None,
            search_result_state: crate::ui::search_result::SearchResultState::default(),
            previous_screen: None,
            clipboard: None,
            file_operation_progress: None,
            pending_tar_archive: None,
            pending_extract_dir: None,
            pending_paste_focus: None,
            conflict_state: None,
            tar_exclude_state: None,
            help_state: HelpState::default(),
            settings_state: None,
            remote_connect_state: None,
            diff_first_panel: None,
            diff_state: None,
            diff_file_view_state: None,
            git_screen_state: None,
            dedup_screen_state: None,
            git_log_diff_state: None,
            pending_remote_open: None,
            remote_spinner: None,
        }
    }

    /// Save current settings to config file
    pub fn save_settings(&mut self) {
        use crate::config::PanelSettings;

        // Preserve extension_handler from current file (user may have edited it externally)
        // Load current file to get the latest extension_handler
        if let Ok(current_file_settings) = Settings::load_with_error() {
            self.settings.extension_handler = current_file_settings.extension_handler;
        }

        // Update settings from current state - save panels array
        let home_path = dirs::home_dir().unwrap_or_else(|| {
            if cfg!(windows) {
                PathBuf::from("C:\\")
            } else {
                PathBuf::from("/")
            }
        });
        self.settings.panels = self
            .panels
            .iter()
            .map(|p| {
                // Remote panel paths should not be saved — use home directory instead
                let path = if p.is_remote() {
                    home_path.display().to_string()
                } else {
                    p.path.display().to_string()
                };
                PanelSettings {
                    start_path: Some(path),
                    sort_by: sort_by_to_string(p.sort_by),
                    sort_order: sort_order_to_string(p.sort_order),
                }
            })
            .collect();
        self.settings.active_panel_index = self.active_panel_index;

        // Save to file (ignore errors silently)
        let _ = self.settings.save();
    }

    /// Reload settings from config file and apply theme
    /// Called when settings.json is edited within the app
    /// Returns true on success, false on error (with error message shown)
    pub fn reload_settings(&mut self) -> bool {
        let new_settings = match Settings::load_with_error() {
            Ok(s) => s,
            Err(e) => {
                self.show_message(&format!("Settings error: {}", e));
                return false;
            }
        };

        // Reload theme if name changed
        if new_settings.theme.name != self.settings.theme.name {
            self.theme = crate::ui::theme::Theme::load(&new_settings.theme.name);
            self.theme_watch_state
                .update_theme(&new_settings.theme.name);
        }

        // Apply panel sort settings from new settings (keep current paths and selection)
        for (i, panel) in self.panels.iter_mut().enumerate() {
            if let Some(ps) = new_settings.panels.get(i) {
                let new_sort_by = parse_sort_by(&ps.sort_by);
                let new_sort_order = parse_sort_order(&ps.sort_order);
                if panel.sort_by != new_sort_by || panel.sort_order != new_sort_order {
                    panel.sort_by = new_sort_by;
                    panel.sort_order = new_sort_order;
                    panel.load_files();
                }
            }
        }

        // Update tar_path setting
        self.settings.tar_path = new_settings.tar_path;

        // Update extension_handler setting
        self.settings.extension_handler = new_settings.extension_handler;

        // Update diff compare method
        self.settings.diff_compare_method = new_settings.diff_compare_method;

        // Update keybindings
        self.keybindings = crate::keybindings::Keybindings::from_config(&new_settings.keybindings);
        self.settings.keybindings = new_settings.keybindings;

        // Update settings
        self.settings.theme = new_settings.theme;
        self.settings.panels = new_settings.panels;

        self.show_message("Settings reloaded");
        true
    }

    /// Check if a path is the settings.json file
    pub fn is_settings_file(path: &std::path::Path) -> bool {
        if let Some(config_path) = Settings::config_path() {
            path == config_path
        } else {
            false
        }
    }

    /// Show settings dialog
    pub fn show_settings_dialog(&mut self) {
        self.settings_state = Some(SettingsState::new(&self.settings));
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Settings,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    /// Apply settings from dialog and save
    pub fn apply_settings_from_dialog(&mut self) {
        if let Some(ref state) = self.settings_state {
            let new_theme_name = state.current_theme().to_string();

            // Update theme if changed
            if new_theme_name != self.settings.theme.name {
                self.settings.theme.name = new_theme_name.clone();
                self.theme = crate::ui::theme::Theme::load(&new_theme_name);
                self.theme_watch_state.update_theme(&new_theme_name);
            }

            // Update diff compare method
            let new_diff_method = state.current_diff_method().to_string();
            self.settings.diff_compare_method = new_diff_method;

            // Save settings
            let _ = self.settings.save();
            self.show_message("Settings saved!");
        }

        self.settings_state = None;
        self.dialog = None;
    }

    /// Cancel settings dialog and restore original theme
    pub fn cancel_settings_dialog(&mut self) {
        // Restore original theme if it was changed during preview
        self.theme = crate::ui::theme::Theme::load(&self.settings.theme.name);
        self.settings_state = None;
        self.dialog = None;
    }

    /// Reload current theme from file (for hot-reload)
    pub fn reload_theme(&mut self) {
        self.theme = crate::ui::theme::Theme::load(&self.settings.theme.name);
    }

    pub fn active_panel_mut(&mut self) -> &mut PanelState {
        &mut self.panels[self.active_panel_index]
    }

    pub fn active_panel(&self) -> &PanelState {
        &self.panels[self.active_panel_index]
    }

    pub fn target_panel(&self) -> &PanelState {
        let target_idx = (self.active_panel_index + 1) % self.panels.len();
        &self.panels[target_idx]
    }

    pub fn switch_panel(&mut self) {
        // 현재 패널의 선택 해제
        self.panels[self.active_panel_index].selected_files.clear();
        self.active_panel_index = (self.active_panel_index + 1) % self.panels.len();
    }

    /// 왼쪽 패널로 전환 (화면 위치 유지)
    pub fn switch_panel_left(&mut self) {
        if self.active_panel_index == 0 {
            return;
        }
        self.switch_panel_keep_index_to(self.active_panel_index - 1);
    }

    /// 오른쪽 패널로 전환 (화면 위치 유지)
    pub fn switch_panel_right(&mut self) {
        if self.active_panel_index >= self.panels.len() - 1 {
            return;
        }
        self.switch_panel_keep_index_to(self.active_panel_index + 1);
    }

    /// 패널 전환 시 화면에서의 상대적 위치(줄 번호) 유지, 새 패널의 스크롤은 변경하지 않음
    fn switch_panel_keep_index_to(&mut self, target_idx: usize) {
        // 현재 패널의 스크롤 오프셋과 선택 인덱스로 화면 내 상대 위치 계산
        let current_scroll = self.panels[self.active_panel_index].scroll_offset;
        let current_index = self.panels[self.active_panel_index].selected_index;
        let relative_pos = current_index.saturating_sub(current_scroll);

        // 현재 패널의 선택 해제
        self.panels[self.active_panel_index].selected_files.clear();

        // 패널 전환
        self.active_panel_index = target_idx;

        // 새 패널의 기존 스크롤 오프셋 유지, 같은 화면 위치에 커서 설정
        let new_panel = &mut self.panels[self.active_panel_index];
        if !new_panel.files.is_empty() {
            let new_scroll = new_panel.scroll_offset;
            let new_total = new_panel.files.len();

            // 새 패널의 스크롤 오프셋 + 화면 내 상대 위치 = 새 선택 인덱스
            let new_index = new_scroll + relative_pos;
            new_panel.selected_index = new_index.min(new_total.saturating_sub(1));
        }
    }

    /// 새 패널 추가
    /// Replace all panels with ones created from the given paths (CLI args)
    pub fn set_panels_from_paths(&mut self, paths: Vec<PathBuf>) {
        let paths: Vec<PathBuf> = paths.into_iter().take(10).collect();
        let panels: Vec<PanelState> = paths.into_iter().map(|p| PanelState::new(p)).collect();
        if !panels.is_empty() {
            self.panels = panels;
            self.active_panel_index = 0;
        }
    }

    pub fn add_panel(&mut self) {
        if self.panels.len() >= 10 {
            return;
        }
        let path = self.active_panel().path.clone();
        let new_panel = PanelState::new(path);
        self.panels.insert(self.active_panel_index + 1, new_panel);
        // AI 인덱스 보정: 삽입 위치보다 뒤에 있으면 +1
        if let Some(ai_idx) = self.ai_panel_index {
            if ai_idx > self.active_panel_index {
                self.ai_panel_index = Some(ai_idx + 1);
            }
        }
        if let Some(prev_idx) = self.ai_previous_panel {
            if prev_idx > self.active_panel_index {
                self.ai_previous_panel = Some(prev_idx + 1);
            }
        }
        // Pending diff first-selection must shift with the insertion too, or it
        // silently refers to the wrong panel afterwards (mirror of close_panel).
        if let Some(first) = self.diff_first_panel {
            if first > self.active_panel_index {
                self.diff_first_panel = Some(first + 1);
            }
        }
        self.active_panel_index += 1;
    }

    /// 현재 패널 닫기
    pub fn close_panel(&mut self) {
        if self.panels.len() <= 1 {
            return;
        }
        let removed_idx = self.active_panel_index;
        // AI가 이 패널에 있으면 AI 상태만 직접 정리 (close_ai_screen은 active_panel_index를 변경하므로 사용하지 않음)
        if self.ai_panel_index == Some(removed_idx) {
            if let Some(ref mut state) = self.ai_state {
                state.save_session_to_file();
            }
            self.ai_panel_index = None;
            self.ai_previous_panel = None;
            self.ai_state = None;
        }
        self.panels.remove(removed_idx);
        // AI 인덱스 보정
        if let Some(ai_idx) = self.ai_panel_index {
            if ai_idx > removed_idx {
                self.ai_panel_index = Some(ai_idx - 1);
            }
        }
        if let Some(prev_idx) = self.ai_previous_panel {
            if prev_idx > removed_idx {
                self.ai_previous_panel = Some(prev_idx - 1);
            } else if prev_idx == removed_idx {
                self.ai_previous_panel = None;
            }
        }
        // Pending diff first-selection index must track the removed panel too,
        // otherwise start_diff() indexes self.panels[first] out of bounds.
        if let Some(first) = self.diff_first_panel {
            if first > removed_idx {
                self.diff_first_panel = Some(first - 1);
            } else if first == removed_idx {
                self.diff_first_panel = None;
            }
        }
        if self.active_panel_index >= self.panels.len() {
            self.active_panel_index = self.panels.len() - 1;
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

    /// Shift+방향키: 현재 항목 토글 후 커서 이동
    pub fn move_cursor_with_selection(&mut self, delta: i32) {
        let panel = self.active_panel_mut();

        // 이동할 새 인덱스 계산
        let new_index = (panel.selected_index as i32 + delta)
            .max(0)
            .min(panel.files.len().saturating_sub(1) as i32) as usize;

        // 이동하지 않는 경우 (이미 맨 위나 맨 아래)
        if new_index == panel.selected_index {
            return;
        }

        // 현재 항목 토글 (".." 제외)
        if let Some(file) = panel.files.get(panel.selected_index) {
            if file.name != ".." {
                let name = file.name.clone();
                if panel.selected_files.contains(&name) {
                    panel.selected_files.remove(&name);
                } else {
                    panel.selected_files.insert(name);
                }
            }
        }

        // 커서 이동
        panel.selected_index = new_index;
    }

    pub fn enter_selected(&mut self) {
        // Check for remote directory navigation first (to avoid borrow conflicts)
        let remote_nav = {
            let panel = &self.panels[self.active_panel_index];
            if let Some(file) = panel.current_file().cloned() {
                if file.is_directory && panel.is_remote() {
                    let (new_path, focus) = if file.name == ".." {
                        let focus = panel
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string());
                        let parent = panel
                            .path
                            .parent()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "/".to_string());
                        (parent, focus)
                    } else {
                        (panel.path.join(&file.name).display().to_string(), None)
                    };
                    Some((new_path, focus))
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some((new_path, focus)) = remote_nav {
            if let Some(focus_name) = focus {
                self.active_panel_mut().pending_focus = Some(focus_name);
            }
            self.spawn_remote_list_dir(&new_path);
            return;
        }

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
                // 원격 파일: 이미지는 뷰어, 나머지는 편집기 (프로그레스 표시)
                if panel.is_remote() {
                    let is_image = {
                        let p = std::path::Path::new(&file.name);
                        crate::ui::image_viewer::is_image_file(p)
                    };

                    if is_image {
                        let tmp_path = match self.remote_tmp_path(&file.name) {
                            Some(p) => p,
                            None => return,
                        };
                        self.download_for_remote_open(
                            &file.name,
                            file.size,
                            PendingRemoteOpen::ImageViewer { tmp_path },
                        );
                    } else {
                        self.edit_file();
                    }
                    return;
                }

                // It's a file - check for extension handler first
                let path = panel.path.join(&file.name);

                // Try extension handler first (takes priority over all default behaviors)
                match self.try_extension_handler(&path) {
                    Ok(true) => {
                        // Handler executed successfully, nothing more to do
                        return;
                    }
                    Ok(false) => {
                        // No handler defined, continue with default behavior
                    }
                    Err(error_msg) => {
                        // All handlers failed, show error dialog
                        self.show_extension_handler_error(&error_msg);
                        return;
                    }
                }

                // Default behavior: check file type
                if Self::is_archive_file(&file.name) {
                    // It's an archive file - extract it
                    self.execute_untar(&path);
                    return;
                }

                // Check file size for large file warning
                const LARGE_FILE_THRESHOLD: u64 = 50 * 1024 * 1024; // 50MB
                let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

                let is_image = crate::ui::image_viewer::is_image_file(&path);

                if file_size > LARGE_FILE_THRESHOLD {
                    // Show confirmation dialog for large file
                    let size_mb = file_size as f64 / (1024.0 * 1024.0);
                    if is_image {
                        self.pending_large_image = Some(path);
                        self.dialog = Some(Dialog {
                            dialog_type: DialogType::LargeImageConfirm,
                            input: String::new(),
                            cursor_pos: 0,
                            message: format!("This image is {:.1}MB. Open anyway?", size_mb),
                            completion: None,
                            selected_button: 1, // Default to "No"
                            selection: None,
                            use_md5: false,
                        });
                    } else {
                        self.pending_large_file = Some(path);
                        self.dialog = Some(Dialog {
                            dialog_type: DialogType::LargeFileConfirm,
                            input: String::new(),
                            cursor_pos: 0,
                            message: format!("This file is {:.1}MB. Open anyway?", size_mb),
                            completion: None,
                            selected_button: 1, // Default to "No"
                            selection: None,
                            use_md5: false,
                        });
                    }
                } else if is_image {
                    // Skip true color check if inline image protocol is available
                    let has_inline = self
                        .image_picker
                        .as_ref()
                        .map(|p| p.protocol_type != ratatui_image::picker::ProtocolType::Halfblocks)
                        .unwrap_or(false);
                    if !has_inline && !crate::ui::image_viewer::supports_true_color() {
                        self.pending_large_image = Some(path);
                        self.dialog = Some(Dialog {
                            dialog_type: DialogType::TrueColorWarning,
                            input: String::new(),
                            cursor_pos: 0,
                            message: "Terminal doesn't support true color. Open anyway?"
                                .to_string(),
                            completion: None,
                            selected_button: 1, // Default to "No"
                            selection: None,
                            use_md5: false,
                        });
                    } else {
                        self.image_viewer_state =
                            Some(crate::ui::image_viewer::ImageViewerState::new(&path));
                        self.current_screen = Screen::ImageViewer;
                    }
                } else {
                    // Regular file - check if binary
                    if Self::is_binary_file(&path) {
                        // Binary file without handler - show handler setup dialog
                        let extension = path
                            .extension()
                            .map(|e| e.to_string_lossy().to_string())
                            .unwrap_or_default();
                        self.pending_binary_file = Some((path, extension.clone()));
                        self.dialog = Some(Dialog {
                            dialog_type: DialogType::BinaryFileHandler,
                            input: String::new(),
                            cursor_pos: 0,
                            message: extension,
                            completion: None,
                            selected_button: 0, // 0: Set mode (no existing handler)
                            selection: None,
                            use_md5: false,
                        });
                    } else {
                        // Text file - open editor
                        self.edit_file()
                    }
                }
            }
        }
    }

    /// Check if a file is a supported archive format
    fn is_archive_file(filename: &str) -> bool {
        let lower = filename.to_lowercase();
        lower.ends_with(".tar")
            || lower.ends_with(".tar.gz")
            || lower.ends_with(".tgz")
            || lower.ends_with(".tar.bz2")
            || lower.ends_with(".tbz2")
            || lower.ends_with(".tar.xz")
            || lower.ends_with(".txz")
    }

    fn process_error_message(
        stderr_output: Option<String>,
        stdout_error_lines: &[String],
        fallback: &str,
    ) -> String {
        let mut parts = Vec::new();

        if let Some(stderr) = stderr_output {
            let stderr = stderr.trim_matches(['\r', '\n']).to_string();
            if !stderr.trim().is_empty() {
                parts.push(stderr);
            }
        }

        if !stdout_error_lines.is_empty() {
            parts.push(stdout_error_lines.join("\n"));
        }

        if parts.is_empty() {
            fallback.to_string()
        } else {
            parts.join("\n")
        }
    }

    /// Check if a file is binary (not a text file)
    /// Reads the first 8KB of the file and checks for null bytes or high proportion of non-text bytes
    fn is_binary_file(path: &std::path::Path) -> bool {
        use std::io::Read;

        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return false, // Can't open, assume text
        };

        let mut reader = std::io::BufReader::new(file);
        let mut buffer = [0u8; 8192]; // Read first 8KB

        let bytes_read = match reader.read(&mut buffer) {
            Ok(n) => n,
            Err(_) => return false,
        };

        if bytes_read == 0 {
            return false; // Empty file is text
        }

        // Check for null bytes (strong indicator of binary)
        // Also count non-printable bytes (excluding common whitespace)
        let mut non_text_count = 0;
        for &byte in &buffer[..bytes_read] {
            if byte == 0 {
                return true; // Null byte = definitely binary
            }
            // Non-printable and non-whitespace characters
            // Allow: tab (9), newline (10), carriage return (13), and printable ASCII (32-126)
            // Also allow UTF-8 continuation bytes (128-255) for international text
            if byte < 9 || (byte > 13 && byte < 32) || byte == 127 {
                non_text_count += 1;
            }
        }

        // If more than 10% of bytes are non-text control characters, consider it binary
        let threshold = bytes_read / 10;
        non_text_count > threshold
    }

    /// Try to execute extension handler commands for a file
    /// Returns Ok(true) if a handler was executed successfully
    /// Returns Ok(false) if no handler is defined for this extension
    /// Returns Err(error_message) if all handlers failed
    ///
    /// Handler prefix:
    /// - No prefix: Foreground execution (suspends TUI, runs command, waits for exit, restores TUI)
    ///   Example: "vim {{FILEPATH}}" - hands over terminal, blocks until program exits
    /// - @ prefix: Background execution (spawns detached, returns to cokacdir immediately)
    ///   Example: "@evince {{FILEPATH}}" - does not wait for program to finish
    pub fn try_extension_handler(&mut self, path: &std::path::Path) -> Result<bool, String> {
        // Get file extension
        let extension = match path.extension() {
            Some(ext) => ext.to_string_lossy().to_string(),
            None => return Ok(false), // No extension, use default behavior
        };

        // Check if there's a handler for this extension
        let handlers = match self.settings.get_extension_handler(&extension) {
            Some(h) => h.clone(),
            None => return Ok(false), // No handler defined, use default behavior
        };

        if handlers.is_empty() {
            return Ok(false);
        }

        // Get the current working directory from active panel
        let cwd = self.active_panel().path.clone();

        let file_path_str = path.to_string_lossy().to_string();
        let mut last_error = String::new();

        // Try each handler in order (fallback mechanism)
        for handler_template in &handlers {
            // Check for background mode prefix (@)
            let (is_background_mode, template) = if handler_template.starts_with('@') {
                (true, &handler_template[1..])
            } else {
                (false, handler_template.as_str())
            };

            // Replace {{FILEPATH}} with actual file path (no escaping needed - will use base64)
            let command = template.replace("{{FILEPATH}}", &file_path_str);

            if is_background_mode {
                // Background mode: spawn and detach (@ prefix)
                match self.execute_background_command(&command, template, &cwd) {
                    Ok(true) => {
                        self.refresh_panels();
                        return Ok(true);
                    }
                    Ok(false) => {
                        // Command failed, error already set in last_error via closure
                        continue;
                    }
                    Err(e) => {
                        last_error = e;
                        continue;
                    }
                }
            } else {
                // Foreground mode: suspend TUI, run command, restore TUI (default)
                match self.execute_terminal_command(&command, &cwd) {
                    Ok(true) => {
                        self.refresh_panels();
                        return Ok(true);
                    }
                    Ok(false) => {
                        last_error = format!("Command failed: {}", template);
                        continue;
                    }
                    Err(e) => {
                        last_error = e;
                        continue;
                    }
                }
            }
        }

        // All handlers failed
        Err(last_error)
    }

    /// Execute a command in terminal mode (blocking, inherits stdio)
    /// Suspends the TUI, runs the command, then restores the TUI
    fn execute_terminal_command(
        &mut self,
        command: &str,
        cwd: &std::path::Path,
    ) -> Result<bool, String> {
        use crossterm::cursor::{Hide, Show};
        use crossterm::execute;
        use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
        use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
        use std::io::{stdout, Write};

        // Show cursor and leave alternate screen
        let _ = execute!(stdout(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();

        // Clear screen for clean command output
        print!("\x1B[2J\x1B[H");
        let _ = stdout().flush();

        // Execute command with inherited stdio and active panel's directory as CWD
        // Use base64 encoding to avoid shell escaping issues
        #[cfg(unix)]
        let result = {
            let encoded = encode_command_base64(command);
            let wrapped_command =
                format!("eval \"$('{}' --base64 '{}')\"", crate::bin_path(), encoded);

            std::process::Command::new("bash")
                .arg("-c")
                .arg(&wrapped_command)
                .current_dir(cwd)
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status()
        };

        #[cfg(windows)]
        let result = std::process::Command::new("cmd")
            .args(["/c", command])
            .current_dir(cwd)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status();

        // Restore: enable raw mode, enter alternate screen, hide cursor
        let _ = enable_raw_mode();
        let _ = execute!(stdout(), EnterAlternateScreen, Hide);

        // Request full redraw on next frame
        self.needs_full_redraw = true;

        match result {
            Ok(status) => Ok(status.success()),
            Err(e) => Err(format!("Failed to execute: {}", e)),
        }
    }

    /// Execute a command in background mode (non-blocking, detached)
    fn execute_background_command(
        &self,
        command: &str,
        template: &str,
        cwd: &std::path::Path,
    ) -> Result<bool, String> {
        #[cfg(unix)]
        let result = {
            // Use base64 encoding to avoid shell escaping issues
            let encoded = encode_command_base64(command);
            let wrapped_command =
                format!("eval \"$('{}' --base64 '{}')\"", crate::bin_path(), encoded);

            std::process::Command::new("bash")
                .arg("-c")
                .arg(&wrapped_command)
                .current_dir(cwd)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
        };

        #[cfg(windows)]
        let result = std::process::Command::new("cmd")
            .args(["/c", command])
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn();

        match result {
            Ok(mut child) => {
                // Wait briefly to check if command started successfully
                std::thread::sleep(std::time::Duration::from_millis(100));

                match child.try_wait() {
                    Ok(Some(status)) => {
                        // Process exited quickly - likely an error
                        if !status.success() {
                            // Try to get stderr
                            if let Some(mut stderr) = child.stderr.take() {
                                use std::io::Read;
                                let mut err_msg = String::new();
                                let _ = stderr.read_to_string(&mut err_msg);
                                if err_msg.trim().is_empty() {
                                    return Err(format!("Command failed: {}", template));
                                } else {
                                    return Err(err_msg.trim().to_string());
                                }
                            }
                            return Err(format!("Command failed: {}", template));
                        }
                        Ok(true) // Command succeeded quickly
                    }
                    Ok(None) => {
                        // Process still running - consider it successful
                        if let Some(stderr) = child.stderr.take() {
                            let _ = std::thread::spawn(move || {
                                use std::io::Read;
                                let mut stderr = stderr;
                                let mut sink = Vec::new();
                                let _ = stderr.read_to_end(&mut sink);
                            });
                        }
                        Ok(true)
                    }
                    Err(e) => Err(format!("Failed to check process: {}", e)),
                }
            }
            Err(e) => Err(format!("Failed to execute '{}': {}", template, e)),
        }
    }

    /// Show extension handler error dialog
    pub fn show_extension_handler_error(&mut self, error_message: &str) {
        self.dialog = Some(Dialog {
            dialog_type: DialogType::ExtensionHandlerError,
            input: String::new(),
            cursor_pos: 0,
            message: error_message.to_string(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    /// Show handler setup dialog for current file (u key)
    pub fn show_handler_dialog(&mut self) {
        if self.active_panel().is_remote() {
            self.show_message("Extension handlers are not available for remote files");
            return;
        }
        let panel = self.active_panel();
        if panel.files.is_empty() {
            return;
        }

        let file = &panel.files[panel.selected_index];
        if file.is_directory {
            return; // No handler for directories
        }

        let path = panel.path.join(&file.name);
        let extension = path
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default();

        if extension.is_empty() {
            self.message = Some("No extension - cannot set handler".to_string());
            self.message_timer = 30;
            return;
        }

        // Check if handler already exists
        let existing_handler = self
            .settings
            .get_extension_handler(&extension)
            .and_then(|handlers| handlers.first().cloned())
            .unwrap_or_default();

        let is_edit_mode = !existing_handler.is_empty();
        let cursor_pos = existing_handler.chars().count();

        // Edit 모드일 때 전체 선택
        let selection = if is_edit_mode {
            Some((0, cursor_pos))
        } else {
            None
        };

        self.pending_binary_file = Some((path, extension.clone()));
        self.dialog = Some(Dialog {
            dialog_type: DialogType::BinaryFileHandler,
            input: existing_handler,
            cursor_pos,
            message: extension,
            completion: None,
            selected_button: if is_edit_mode { 1 } else { 0 }, // 0: Set, 1: Edit
            selection,
            use_md5: false,
        });
    }

    pub fn go_to_parent(&mut self) {
        if self.active_panel().is_remote() {
            // Remote parent navigation — use spinner
            let focus = self
                .active_panel()
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string());
            let parent = self
                .active_panel()
                .path
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "/".to_string());
            if let Some(focus_name) = focus {
                self.active_panel_mut().pending_focus = Some(focus_name);
            }
            self.spawn_remote_list_dir(&parent);
            return;
        }
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

    /// 홈 디렉토리로 이동
    pub fn goto_home(&mut self) {
        if let Some(home) = dirs::home_dir() {
            // Disconnect remote if active panel is remote
            if self.active_panel().is_remote() {
                if self.remote_spinner.is_some() {
                    return;
                }
                self.disconnect_remote_panel();
            }
            let panel = self.active_panel_mut();
            panel.path = home;
            panel.selected_index = 0;
            panel.selected_files.clear();
            panel.load_files();
        }
    }

    /// Open current folder in Finder (macOS only)
    #[cfg(target_os = "macos")]
    pub fn open_in_finder(&mut self) {
        let path = self.active_panel().path.clone();
        match std::process::Command::new("open").arg(&path).spawn() {
            Ok(_) => self.show_message(&format!("Opened in Finder: {}", path.display())),
            Err(e) => self.show_message(&format!("Failed to open: {}", e)),
        }
    }

    /// Open current folder in VS Code (macOS only)
    /// Falls back to code-insiders if code is not available
    #[cfg(target_os = "macos")]
    pub fn open_in_vscode(&mut self) {
        use std::process::Command;

        let path = self.active_panel().path.clone();

        // Check which command is available
        let code_cmd = if Command::new("which")
            .arg("code")
            .output()
            .map(|o| {
                if !o.status.success() {
                    return false;
                }
                let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
                !p.is_empty() && std::path::Path::new(&p).exists()
            })
            .unwrap_or(false)
        {
            "code"
        } else if Command::new("which")
            .arg("code-insiders")
            .output()
            .map(|o| {
                if !o.status.success() {
                    return false;
                }
                let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
                !p.is_empty() && std::path::Path::new(&p).exists()
            })
            .unwrap_or(false)
        {
            "code-insiders"
        } else {
            self.show_message("VS Code not found (tried: code, code-insiders)");
            return;
        };

        match Command::new(code_cmd).arg(&path).spawn() {
            Ok(_) => self.show_message(&format!("Opened in {}: {}", code_cmd, path.display())),
            Err(e) => self.show_message(&format!("Failed to open {}: {}", code_cmd, e)),
        }
    }

    /// Open current folder in Explorer (Windows only)
    #[cfg(target_os = "windows")]
    pub fn open_in_explorer(&mut self) {
        let path = self.active_panel().path.clone();
        match std::process::Command::new("explorer").arg(&path).spawn() {
            Ok(_) => self.show_message(&format!("Opened in Explorer: {}", path.display())),
            Err(e) => self.show_message(&format!("Failed to open: {}", e)),
        }
    }

    /// Open current folder in VS Code (Windows only)
    /// Falls back to code-insiders if code is not available
    #[cfg(target_os = "windows")]
    pub fn open_in_vscode_win(&mut self) {
        use std::process::Command;

        let path = self.active_panel().path.clone();

        let code_cmd = if Command::new("where")
            .arg("code")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            "code"
        } else if Command::new("where")
            .arg("code-insiders")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            "code-insiders"
        } else {
            self.show_message("VS Code not found (tried: code, code-insiders)");
            return;
        };

        match Command::new(code_cmd).arg(&path).spawn() {
            Ok(_) => self.show_message(&format!("Opened in {}: {}", code_cmd, path.display())),
            Err(e) => self.show_message(&format!("Failed to open {}: {}", code_cmd, e)),
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

    pub fn select_by_extension(&mut self) {
        let panel = self.active_panel_mut();
        if let Some(current_file) = panel.files.get(panel.selected_index) {
            // Get extension of current file
            let target_ext = std::path::Path::new(&current_file.name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase());

            if let Some(ext) = target_ext {
                // Collect files with same extension
                let matching_files: Vec<String> = panel
                    .files
                    .iter()
                    .filter(|f| f.name != ".." && !f.is_directory)
                    .filter(|f| {
                        std::path::Path::new(&f.name)
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e.to_lowercase())
                            .as_ref()
                            == Some(&ext)
                    })
                    .map(|f| f.name.clone())
                    .collect();

                // Check if all matching files are already selected
                let all_selected = matching_files
                    .iter()
                    .all(|name| panel.selected_files.contains(name));

                let count = matching_files.len();
                if all_selected {
                    // Deselect all matching files
                    for name in matching_files {
                        panel.selected_files.remove(&name);
                    }
                    self.show_message(&format!("Deselected {} .{} file(s)", count, ext));
                } else {
                    // Select all matching files
                    for name in matching_files {
                        panel.selected_files.insert(name);
                    }
                    self.show_message(&format!("Selected {} .{} file(s)", count, ext));
                }
            }
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

    pub fn toggle_sort_by_type(&mut self) {
        self.active_panel_mut().toggle_sort(SortBy::Type);
    }

    pub fn show_message(&mut self, msg: &str) {
        self.message = Some(msg.to_string());
        self.message_timer = 10; // ~1 second at 10 FPS
    }

    /// Toggle bookmark for the current panel's path
    pub fn toggle_bookmark(&mut self) {
        let current_path = if self.active_panel().is_remote() {
            let path = self.active_panel().path.display().to_string();
            if let Some(ref ctx) = self.active_panel().remote_ctx {
                remote::format_remote_display(&ctx.profile, &path)
            } else if let Some((ref user, ref host, port)) = self.active_panel().remote_display {
                if port != 22 {
                    format!("{}@{}:{}:{}", user, host, port, path)
                } else {
                    format!("{}@{}:{}", user, host, path)
                }
            } else {
                return;
            }
        } else {
            self.active_panel().path.display().to_string()
        };

        if let Some(pos) = self
            .settings
            .bookmarked_path
            .iter()
            .position(|p| p == &current_path)
        {
            self.settings.bookmarked_path.remove(pos);
            self.show_message(&format!("Bookmark removed: {}", current_path));
        } else {
            self.settings.bookmarked_path.push(current_path.clone());
            self.show_message(&format!("Bookmark added: {}", current_path));
        }

        let _ = self.settings.save();
    }

    pub fn refresh_panels(&mut self) {
        // Check if any panel is remote and needs async refresh
        let mut remote_panel_idx = None;
        for (i, panel) in self.panels.iter_mut().enumerate() {
            panel.selected_files.clear();
            if panel.is_remote() {
                if panel.remote_ctx.is_some() {
                    // Don't call load_files on remote panels — use spinner instead
                    remote_panel_idx = Some(i);
                }
                // If remote_ctx is temporarily taken by background thread, skip
            } else {
                panel.load_files();
            }
        }
        // Spawn async refresh for the first remote panel found
        if let Some(idx) = remote_panel_idx {
            if self.remote_spinner.is_none() {
                self.spawn_remote_refresh(idx);
            }
        }
    }

    /// Start diff comparison between panels
    /// With 2 panels: immediately enter diff screen
    /// With 3+ panels: first call selects first panel, second call selects second panel
    pub fn start_diff(&mut self) {
        if self.panels.iter().any(|p| p.is_remote()) {
            self.show_message("Diff is not supported for remote panels");
            return;
        }

        // Priority: if exactly 2 directories are selected in active panel, diff them
        let panel = &self.panels[self.active_panel_index];
        let selected_dirs: Vec<PathBuf> = panel
            .files
            .iter()
            .filter(|f| f.is_directory && panel.selected_files.contains(&f.name))
            .map(|f| panel.path.join(&f.name))
            .collect();
        if selected_dirs.len() == 2 {
            let left = selected_dirs[0].clone();
            let right = selected_dirs[1].clone();
            self.panels[self.active_panel_index].selected_files.clear();
            self.enter_diff_screen(left, right);
            return;
        }

        if self.panels.len() < 2 {
            self.show_message("Need at least 2 panels for diff");
            return;
        }

        if self.panels.len() == 2 {
            // 2 panels: immediate diff
            let left = self.panels[0].path.clone();
            let right = self.panels[1].path.clone();
            self.enter_diff_screen(left, right);
        } else {
            // 3+ panels: 2-stage selection
            if let Some(first) = self.diff_first_panel.filter(|&f| f < self.panels.len()) {
                // Second selection
                let second = self.active_panel_index;
                if first == second {
                    self.show_message("Select a different panel for diff");
                    return;
                }
                let left = self.panels[first].path.clone();
                let right = self.panels[second].path.clone();
                self.diff_first_panel = None;
                self.enter_diff_screen(left, right);
            } else {
                // First selection
                self.diff_first_panel = Some(self.active_panel_index);
                let diff_key = self
                    .keybindings
                    .panel_first_key(crate::keybindings::PanelAction::StartDiff);
                let cancel_key = self
                    .keybindings
                    .panel_first_key(crate::keybindings::PanelAction::ParentDir);
                self.show_message(&format!(
                    "Select second panel for diff ({}) or {} to cancel",
                    diff_key, cancel_key
                ));
            }
        }
    }

    /// Enter diff screen with two directory paths
    pub fn enter_diff_screen(&mut self, left: PathBuf, right: PathBuf) {
        if left == right {
            self.show_message("Both paths are the same");
            return;
        }
        let compare_method =
            crate::ui::diff_screen::parse_compare_method(&self.settings.diff_compare_method);
        let sort_by = self.active_panel().sort_by;
        let sort_order = self.active_panel().sort_order;
        let mut state = crate::ui::diff_screen::DiffState::new(
            left,
            right,
            compare_method,
            sort_by,
            sort_order,
        );
        state.start_comparison();
        self.diff_state = Some(state);
        self.current_screen = Screen::DiffScreen;
    }

    /// Enter file content diff view from the diff screen
    pub fn enter_diff_file_view(
        &mut self,
        left_path: PathBuf,
        right_path: PathBuf,
        file_name: String,
    ) {
        self.diff_file_view_state = Some(crate::ui::diff_file_view::DiffFileViewState::new(
            left_path, right_path, file_name,
        ));
        self.current_screen = Screen::DiffFileView;
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

    /// Calculate total size and build file size map for tar progress
    fn calculate_tar_sizes(
        base_dir: &Path,
        files: &[String],
    ) -> (u64, std::collections::HashMap<String, u64>) {
        use std::collections::HashMap;
        let mut total_size = 0u64;
        let mut size_map = HashMap::new();

        for file in files {
            let path = base_dir.join(file);
            Self::collect_file_sizes(
                &path,
                &format!("./{}", file),
                &mut size_map,
                &mut total_size,
            );
        }

        (total_size, size_map)
    }

    /// Collect file sizes recursively, matching tar's output format
    fn collect_file_sizes(
        path: &Path,
        tar_path: &str,
        size_map: &mut std::collections::HashMap<String, u64>,
        total_size: &mut u64,
    ) {
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            if metadata.is_dir() {
                // Directory itself (tar lists directories too)
                size_map.insert(tar_path.to_string(), 0);

                if let Ok(entries) = std::fs::read_dir(path) {
                    for entry in entries.filter_map(|e| e.ok()) {
                        let entry_name = entry.file_name().to_string_lossy().to_string();
                        let child_tar_path = format!("{}/{}", tar_path, entry_name);
                        Self::collect_file_sizes(
                            &entry.path(),
                            &child_tar_path,
                            size_map,
                            total_size,
                        );
                    }
                }
            } else {
                // Regular file or symlink
                let size = metadata.len();
                size_map.insert(tar_path.to_string(), size);
                *total_size += size;
            }
        }
    }

    // Dialog methods
    pub fn show_help(&mut self) {
        self.current_screen = Screen::Help;
    }

    pub fn show_file_info(&mut self) {
        if self.active_panel().is_remote() {
            self.show_message("File info is not available for remote files");
            return;
        }
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
        if self.active_panel().is_remote() {
            self.show_message("Cannot view remote files directly. Use copy to download first.");
            return;
        }
        let panel = self.active_panel();
        if let Some(file) = panel.current_file() {
            if !file.is_directory {
                let path = panel.path.join(&file.name);

                // Check if it's an image file
                if crate::ui::image_viewer::is_image_file(&path) {
                    // Skip true color check if inline image protocol is available
                    let has_inline = self
                        .image_picker
                        .as_ref()
                        .map(|p| p.protocol_type != ratatui_image::picker::ProtocolType::Halfblocks)
                        .unwrap_or(false);
                    if !has_inline && !crate::ui::image_viewer::supports_true_color() {
                        self.pending_large_image = Some(path);
                        self.dialog = Some(Dialog {
                            dialog_type: DialogType::TrueColorWarning,
                            input: String::new(),
                            cursor_pos: 0,
                            message: "Terminal doesn't support true color. Open anyway?"
                                .to_string(),
                            completion: None,
                            selected_button: 1, // Default to "No"
                            selection: None,
                            use_md5: false,
                        });
                        return;
                    }

                    // Check file size (threshold: 50MB)
                    const LARGE_IMAGE_THRESHOLD: u64 = 50 * 1024 * 1024;
                    let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

                    if file_size > LARGE_IMAGE_THRESHOLD {
                        // Show confirmation dialog for large image
                        let size_mb = file_size as f64 / (1024.0 * 1024.0);
                        self.pending_large_image = Some(path);
                        self.dialog = Some(Dialog {
                            dialog_type: DialogType::LargeImageConfirm,
                            input: String::new(),
                            cursor_pos: 0,
                            message: format!("This image is {:.1}MB. Open anyway?", size_mb),
                            completion: None,
                            selected_button: 1, // Default to "No"
                            selection: None,
                            use_md5: false,
                        });
                        return;
                    }

                    self.image_viewer_state =
                        Some(crate::ui::image_viewer::ImageViewerState::new(&path));
                    self.current_screen = Screen::ImageViewer;
                    return;
                }

                // 새로운 고급 뷰어 사용
                let mut viewer = ViewerState::new();
                viewer.set_syntax_colors(self.theme.syntax);
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

    /// 원격 파일의 로컬 tmp 경로 생성
    fn remote_tmp_path(&self, file_name: &str) -> Option<PathBuf> {
        let panel = self.active_panel();
        let remote_path = format!("{}/{}", panel.path.display(), file_name);
        if let Some(ref ctx) = panel.remote_ctx {
            let home = dirs::home_dir()?;
            let tmp_base = home
                .join(".cokacdir")
                .join("tmp")
                .join(format!("{}@{}", ctx.profile.user, ctx.profile.host));
            Some(tmp_base.join(remote_path.trim_start_matches('/')))
        } else {
            None
        }
    }

    /// 원격 파일을 tmp로 다운로드 (프로그레스 표시) 후 편집기/뷰어로 열기
    fn download_for_remote_open(
        &mut self,
        file_name: &str,
        file_size: u64,
        open_action: PendingRemoteOpen,
    ) {
        let panel_index = self.active_panel_index;
        let panel = &self.panels[panel_index];
        let remote_path = format!("{}/{}", panel.path.display(), file_name);

        let (profile, tmp_path) = if let Some(ref ctx) = panel.remote_ctx {
            let Some(home) = dirs::home_dir() else {
                return;
            };
            let tmp_base = home
                .join(".cokacdir")
                .join("tmp")
                .join(format!("{}@{}", ctx.profile.user, ctx.profile.host));
            let tmp_path = tmp_base.join(remote_path.trim_start_matches('/'));
            (ctx.profile.clone(), tmp_path)
        } else {
            return;
        };

        // 디렉토리 생성
        if let Some(parent) = tmp_path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                self.show_message(&format!("Cannot create tmp dir: {}", e));
                return;
            }
        }

        // 프로그레스 설정
        let mut progress = FileOperationProgress::new(file_ops::FileOperationType::Download);
        progress.is_active = true;
        let cancel_flag = progress.cancel_flag.clone();
        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        let tmp_path_clone = tmp_path.clone();
        let remote_path_clone = remote_path.clone();
        let file_name_owned = file_name.to_string();

        thread::spawn(move || {
            let _ = tx.send(file_ops::ProgressMessage::Preparing(format!(
                "Connecting to {}...",
                profile.host
            )));

            // 새 SFTP 세션 연결
            let session = match remote::SftpSession::connect(&profile) {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(file_ops::ProgressMessage::Error(
                        file_name_owned.clone(),
                        format!("Connection failed: {}", e),
                    ));
                    let _ = tx.send(file_ops::ProgressMessage::Completed(0, 1));
                    return;
                }
            };

            let _ = tx.send(file_ops::ProgressMessage::PrepareComplete);
            let _ = tx.send(file_ops::ProgressMessage::FileStarted(
                file_name_owned.clone(),
            ));
            let _ = tx.send(file_ops::ProgressMessage::TotalProgress(0, 1, 0, file_size));

            // 프로그레스 콜백과 함께 다운로드
            let local_path_str = tmp_path_clone.display().to_string();
            match session.download_file_with_progress(
                &remote_path_clone,
                &local_path_str,
                file_size,
                &cancel_flag,
                |downloaded, total| {
                    let _ = tx.send(file_ops::ProgressMessage::FileProgress(downloaded, total));
                    let _ = tx.send(file_ops::ProgressMessage::TotalProgress(
                        0, 1, downloaded, total,
                    ));
                },
            ) {
                Ok(_) => {
                    let _ = tx.send(file_ops::ProgressMessage::FileCompleted(file_name_owned));
                    let _ = tx.send(file_ops::ProgressMessage::TotalProgress(
                        1, 1, file_size, file_size,
                    ));
                    let _ = tx.send(file_ops::ProgressMessage::Completed(1, 0));
                }
                Err(e) => {
                    let _ = tx.send(file_ops::ProgressMessage::Error(file_name_owned, e));
                    let _ = tx.send(file_ops::ProgressMessage::Completed(0, 1));
                }
            }
        });

        self.pending_remote_open = Some(open_action);
        self.file_operation_progress = Some(progress);
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Progress,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    pub fn edit_file(&mut self) {
        if self.active_panel().is_remote() {
            let panel = self.active_panel();
            let file = match panel.current_file() {
                Some(f) if !f.is_directory => f.clone(),
                Some(_) => {
                    self.show_message("Select a file to edit");
                    return;
                }
                None => return,
            };
            let remote_path = format!("{}/{}", panel.path.display(), file.name);
            let panel_index = self.active_panel_index;
            let tmp_path = self.remote_tmp_path(&file.name);
            let tmp_path = match tmp_path {
                Some(p) => p,
                None => return,
            };
            self.download_for_remote_open(
                &file.name,
                file.size,
                PendingRemoteOpen::Editor {
                    tmp_path,
                    panel_index,
                    remote_path,
                },
            );
        } else {
            // 로컬 파일: 기존 로직
            let panel = self.active_panel();
            if let Some(file) = panel.current_file() {
                if !file.is_directory {
                    let path = panel.path.join(&file.name);

                    let mut editor = EditorState::new();
                    editor.set_syntax_colors(self.theme.syntax);
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
            cursor_pos: 0,
            message: format!("Delete {}?", file_list),
            completion: None,
            selected_button: 1, // 기본값: No (안전을 위해)
            selection: None,
            use_md5: false,
        });
    }

    pub fn show_encrypt_dialog(&mut self) {
        if self.active_panel().is_remote() {
            self.show_message("Encryption is not available on remote panels");
            return;
        }

        let dir = self.active_panel().path.clone();
        let count = match fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let path = e.path();
                    if !path.is_file() {
                        return false;
                    }
                    let name = e.file_name().to_string_lossy().to_string();
                    !name.ends_with(".cokacenc") && !name.starts_with('.')
                })
                .count(),
            Err(_) => 0,
        };

        if count == 0 {
            self.show_message("No files to encrypt");
            return;
        }

        let split_size = self.settings.encrypt_split_size.to_string();
        let cursor = split_size.len();
        self.dialog = Some(Dialog {
            dialog_type: DialogType::EncryptConfirm,
            input: split_size,
            cursor_pos: cursor,
            message: format!("Encrypt {} file(s)? Split size MB (0=no split):", count),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    pub fn show_decrypt_dialog(&mut self) {
        if self.active_panel().is_remote() {
            self.show_message("Decryption is not available on remote panels");
            return;
        }

        let dir = self.active_panel().path.clone();
        let count = match fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let path = e.path();
                    path.is_file() && e.file_name().to_string_lossy().ends_with(".cokacenc")
                })
                .count(),
            Err(_) => 0,
        };

        if count == 0 {
            self.show_message("No .cokacenc files to decrypt");
            return;
        }

        self.dialog = Some(Dialog {
            dialog_type: DialogType::DecryptConfirm,
            input: String::new(),
            cursor_pos: 0,
            message: format!("Decrypt {} .cokacenc file(s) in {}?", count, dir.display()),
            completion: None,
            selected_button: 1, // Default: No
            selection: None,
            use_md5: false,
        });
    }

    pub fn execute_encrypt(&mut self, split_size_mb: u64, use_md5: bool) {
        // Remember split size for next time
        self.settings.encrypt_split_size = split_size_mb;

        let key_path = match crate::enc::ensure_key() {
            Ok(p) => p,
            Err(e) => {
                self.show_message(&format!("Key error: {}", e));
                return;
            }
        };

        let dir = self.active_panel().path.clone();

        let mut progress = FileOperationProgress::new(FileOperationType::Encrypt);
        progress.is_active = true;
        let cancel_flag = progress.cancel_flag.clone();

        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        thread::spawn(move || {
            crate::enc::pack_directory_with_progress(
                &dir,
                &key_path,
                tx,
                cancel_flag,
                split_size_mb,
                use_md5,
            );
        });

        self.file_operation_progress = Some(progress);
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Progress,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    pub fn execute_decrypt(&mut self) {
        let key_path = match crate::enc::ensure_key() {
            Ok(p) => p,
            Err(e) => {
                self.show_message(&format!("Key error: {}", e));
                return;
            }
        };

        let dir = self.active_panel().path.clone();

        let mut progress = FileOperationProgress::new(FileOperationType::Decrypt);
        progress.is_active = true;
        let cancel_flag = progress.cancel_flag.clone();

        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        thread::spawn(move || {
            crate::enc::unpack_directory_with_progress(&dir, &key_path, tx, cancel_flag);
        });

        self.file_operation_progress = Some(progress);
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Progress,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    pub fn show_mkdir_dialog(&mut self) {
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Mkdir,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    pub fn show_mkfile_dialog(&mut self) {
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Mkfile,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    pub fn show_rename_dialog(&mut self) {
        let panel = self.active_panel();
        if let Some(file) = panel.current_file() {
            if file.name != ".." {
                let name = &file.name;
                let len = name.chars().count();

                // 확장자 제외한 선택 범위 계산
                // 디렉토리: 전체 선택
                // 파일: 마지막 '.' 앞까지 선택 (숨김파일 고려)
                let selection_end = if file.is_directory {
                    len
                } else {
                    // 숨김 파일(.으로 시작)의 경우 첫 번째 점 이후의 확장자만 찾음
                    let search_start = if name.starts_with('.') { 1 } else { 0 };
                    if let Some(dot_pos) = name[search_start..].rfind('.') {
                        // 확장자가 있으면 그 앞까지
                        name[..search_start].chars().count()
                            + name[search_start..search_start + dot_pos].chars().count()
                    } else {
                        // 확장자 없으면 전체
                        len
                    }
                };

                self.dialog = Some(Dialog {
                    dialog_type: DialogType::Rename,
                    input: file.name.clone(),
                    cursor_pos: selection_end,
                    message: String::new(),
                    completion: None,
                    selected_button: 0,
                    selection: Some((0, selection_end)),
                    use_md5: false,
                });
            } else {
                self.show_message("Select a file to rename");
            }
        }
    }

    pub fn show_tar_dialog(&mut self) {
        let files = self.get_operation_files();
        if files.is_empty() {
            self.show_message("No files selected");
            return;
        }

        // Generate default archive name based on first file
        let first_file = &files[0];
        let archive_name = format!("{}.tar", first_file);

        let file_list = if files.len() <= 3 {
            files.join(", ")
        } else {
            format!("{} and {} more", files[..2].join(", "), files.len() - 2)
        };

        let cursor_pos = archive_name.chars().count();
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Tar,
            input: archive_name,
            cursor_pos,
            message: file_list,
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    pub fn show_search_dialog(&mut self) {
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Search,
            input: String::new(),
            cursor_pos: 0,
            message: "Search for:".to_string(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    pub fn show_goto_dialog(&mut self) {
        let current_path = self.active_panel().display_path();
        let len = current_path.chars().count();
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Goto,
            input: current_path,
            cursor_pos: len,
            message: "Go to path:".to_string(),
            completion: Some(PathCompletion::default()),
            selected_button: 0,
            selection: Some((0, len)), // 전체 선택
            use_md5: false,
        });
    }

    pub fn show_process_manager(&mut self) {
        self.processes = crate::services::process::get_process_list();
        self.process_selected_index = 0;
        self.process_confirm_kill = None;
        self.current_screen = Screen::ProcessManager;
    }

    pub fn show_ai_screen(&mut self) {
        if self.active_panel().is_remote() {
            self.show_message("AI features are not available for remote panels");
            return;
        }
        // 1패널이면 AI용 패널 자동 추가
        if self.panels.len() == 1 {
            let path = self.active_panel().path.clone();
            self.panels.push(PanelState::new(path));
        }
        let current_path = self.active_panel().path.display().to_string();
        // Try to load the most recent session, fall back to new session
        // Note: claude availability is checked inside AIScreenState (displays error in UI if unavailable)
        self.ai_state = Some(
            crate::ui::ai_screen::AIScreenState::load_latest_session(current_path.clone())
                .unwrap_or_else(|| crate::ui::ai_screen::AIScreenState::new(current_path)),
        );
        // 원래 포커스 위치 저장
        self.ai_previous_panel = Some(self.active_panel_index);
        // AI 화면을 비활성 패널(다음 패널)에 표시
        let ai_idx = (self.active_panel_index + 1) % self.panels.len();
        self.ai_panel_index = Some(ai_idx);
        // 포커스를 AI 화면으로 이동
        self.active_panel_index = ai_idx;
    }

    /// AI 화면을 닫고 상태 초기화
    pub fn close_ai_screen(&mut self) {
        if let Some(ref mut state) = self.ai_state {
            state.save_session_to_file();
        }
        // 원래 포커스 위치로 복원
        if let Some(prev) = self.ai_previous_panel {
            if prev < self.panels.len() {
                self.active_panel_index = prev;
            }
        }
        self.ai_panel_index = None;
        self.ai_previous_panel = None;
        self.ai_state = None;
        self.refresh_panels();
    }

    /// AI 모드가 활성화되어 있는지 확인
    pub fn is_ai_mode(&self) -> bool {
        self.ai_panel_index.is_some() && self.ai_state.is_some()
    }

    pub fn show_system_info(&mut self) {
        self.system_info_state = crate::ui::system_info::SystemInfoState::default();
        self.current_screen = Screen::SystemInfo;
    }

    pub fn show_git_screen(&mut self) {
        let path = self.active_panel().path.clone();
        if !crate::ui::git_screen::is_git_repo(&path) {
            self.show_message("Not a git repository");
            return;
        }
        self.git_screen_state = Some(crate::ui::git_screen::GitScreenState::new(path));
        self.current_screen = Screen::GitScreen;
    }

    pub fn show_dedup_screen(&mut self) {
        let path = self.active_panel().path.clone();
        self.dialog = Some(Dialog {
            dialog_type: DialogType::DedupConfirm,
            input: String::new(),
            cursor_pos: 0,
            message: format!("WARNING: This will PERMANENTLY DELETE duplicate files in {}. This action cannot be undone. Proceed?", path.display()),
            completion: None,
            selected_button: 1,  // Default: No
            selection: None,
            use_md5: false,
        });
    }

    pub fn execute_dedup(&mut self) {
        let path = self.active_panel().path.clone();
        self.dedup_screen_state = Some(crate::ui::dedup_screen::DedupScreenState::new(path));
        self.current_screen = Screen::DedupScreen;
    }

    pub fn show_git_log_diff_dialog(&mut self) {
        let path = self.active_panel().path.clone();
        if !crate::ui::git_screen::is_git_repo(&path) {
            self.show_message("Not a git repository");
            return;
        }
        let repo_root = match crate::ui::git_screen::get_repo_root(&path) {
            Some(r) => r,
            None => {
                self.show_message("Failed to get git repo root");
                return;
            }
        };
        let project_name = repo_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string());
        let log_entries = crate::ui::git_screen::get_log_public(&repo_root, 200);
        if log_entries.is_empty() {
            self.show_message("No git commits found");
            return;
        }
        self.git_log_diff_state = Some(GitLogDiffState {
            repo_path: repo_root,
            project_name,
            log_entries,
            selected_index: 0,
            scroll_offset: 0,
            selected_commits: Vec::new(),
            visible_height: 20,
        });
        self.dialog = Some(Dialog {
            dialog_type: DialogType::GitLogDiff,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    pub fn execute_git_log_diff(&mut self) {
        self.dialog = None;

        let state = match self.git_log_diff_state.take() {
            Some(s) => s,
            None => return,
        };
        if state.selected_commits.len() != 2 {
            return;
        }
        let hash1 = state.selected_commits[0].clone();
        let hash2 = state.selected_commits[1].clone();

        // Validate hashes
        if !hash1.chars().all(|c| c.is_ascii_alphanumeric())
            || !hash2.chars().all(|c| c.is_ascii_alphanumeric())
        {
            self.show_message("Invalid commit hash");
            return;
        }

        if self.remote_spinner.is_some() {
            return;
        }

        let project_name = state.project_name.clone();
        let repo_path = state.repo_path.clone();
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let Some(home) = dirs::home_dir() else {
                let _ = tx.send(RemoteSpinnerResult::GitDiffComplete {
                    result: Err("Failed to get home directory".to_string()),
                });
                return;
            };
            let diff_base = home.join(".cokacdir").join("diff");

            let _ = std::fs::remove_dir_all(&diff_base);
            if std::fs::create_dir_all(&diff_base).is_err() {
                let _ = tx.send(RemoteSpinnerResult::GitDiffComplete {
                    result: Err("Failed to create diff directory".to_string()),
                });
                return;
            }

            let dir1 = diff_base.join(format!("{}_{}", project_name, hash1));
            let dir2 = diff_base.join(format!("{}_{}", project_name, hash2));

            for (dir, hash) in [(&dir1, &hash1), (&dir2, &hash2)] {
                let repo_str = repo_path.display().to_string();
                let dir_str = dir.display().to_string();
                #[cfg(unix)]
                let status = std::process::Command::new("cp")
                    .args(["-a", &repo_str, &dir_str])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                #[cfg(windows)]
                let status = std::process::Command::new("xcopy")
                    .args([&repo_str, &dir_str, "/e", "/h", "/k", "/q", "/y", "/i"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                if status.map(|s| !s.success()).unwrap_or(true) {
                    let _ = std::fs::remove_dir_all(&diff_base);
                    let _ = tx.send(RemoteSpinnerResult::GitDiffComplete {
                        result: Err("Failed to copy repository".to_string()),
                    });
                    return;
                }

                let checkout_status = crate::ui::git_screen::git_cmd_public(dir)
                    .args(["checkout", hash.as_str()])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                if checkout_status.map(|s| !s.success()).unwrap_or(true) {
                    let _ = std::fs::remove_dir_all(&diff_base);
                    let _ = tx.send(RemoteSpinnerResult::GitDiffComplete {
                        result: Err(format!("Failed to checkout {}", hash)),
                    });
                    return;
                }

                let _ = std::fs::remove_dir_all(dir.join(".git"));
            }

            let _ = tx.send(RemoteSpinnerResult::GitDiffComplete {
                result: Ok((dir1, dir2)),
            });
        });

        self.remote_spinner = Some(RemoteSpinner {
            message: "Preparing diff...".to_string(),
            started_at: Instant::now(),
            receiver: rx,
        });
    }

    #[allow(dead_code)]
    pub fn show_advanced_search_dialog(&mut self) {
        self.advanced_search_state.active = true;
        self.advanced_search_state.reset();
    }

    pub fn execute_advanced_search(
        &mut self,
        criteria: &crate::ui::advanced_search::SearchCriteria,
    ) {
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

    pub fn execute_delete(&mut self) {
        // 이미지 뷰어에서 삭제 시 현재 보고 있는 이미지 삭제
        if self.current_screen == Screen::ImageViewer {
            if let Some(ref state) = self.image_viewer_state {
                let path = state.path.clone();
                match file_ops::delete_file(&path) {
                    Ok(_) => {
                        self.show_message("Deleted image");
                        // 이미지 뷰어 닫기
                        self.current_screen = Screen::FilePanel;
                        self.image_viewer_state = None;
                    }
                    Err(e) => {
                        self.show_message(&format!("Delete failed: {}", e));
                    }
                }
                self.refresh_panels();
            }
            return;
        }

        let files = self.get_operation_files();
        let source_path = self.active_panel().path.clone();
        let is_remote = self.active_panel().is_remote();

        let mut success_count = 0;
        let mut last_error = String::new();

        if is_remote {
            // Remote delete via SFTP (async with spinner)
            if self.remote_spinner.is_some() {
                return;
            }
            let panel_idx = self.active_panel_index;
            let mut ctx = match self.panels[panel_idx].remote_ctx.take() {
                Some(ctx) => ctx,
                None => return,
            };
            let remote_base = source_path.display().to_string();
            // Collect file info before spawning thread
            let file_infos: Vec<(String, bool)> = files
                .iter()
                .map(|file_name| {
                    let is_dir = self
                        .active_panel()
                        .files
                        .iter()
                        .find(|f| f.name == *file_name)
                        .map(|f| f.is_directory)
                        .unwrap_or(false);
                    (file_name.clone(), is_dir)
                })
                .collect();
            let total = file_infos.len();
            let (tx, rx) = mpsc::channel();

            thread::spawn(move || {
                let mut success_count = 0;
                let mut last_error = String::new();
                for (file_name, is_dir) in &file_infos {
                    let remote_path =
                        format!("{}/{}", remote_base.trim_end_matches('/'), file_name);
                    match ctx.session.remove(&remote_path, *is_dir) {
                        Ok(_) => success_count += 1,
                        Err(e) => last_error = e,
                    }
                }
                let msg = if success_count == total {
                    Ok(format!("Deleted {} file(s)", success_count))
                } else {
                    Err(format!(
                        "Deleted {}/{}. Error: {}",
                        success_count, total, last_error
                    ))
                };
                let _ = tx.send(RemoteSpinnerResult::PanelOp {
                    ctx,
                    panel_idx,
                    outcome: PanelOpOutcome::Simple {
                        message: msg,
                        pending_focus: None,
                        reload: true,
                    },
                });
            });

            self.remote_spinner = Some(RemoteSpinner {
                message: "Deleting...".to_string(),
                started_at: Instant::now(),
                receiver: rx,
            });
            return;
        } else {
            // Local delete in background thread with spinner
            if self.remote_spinner.is_some() {
                return;
            }
            let files_to_delete: Vec<PathBuf> = files.iter().map(|f| source_path.join(f)).collect();
            let total = files_to_delete.len();
            let (tx, rx) = mpsc::channel();

            thread::spawn(move || {
                let mut success_count = 0;
                let mut last_error = String::new();
                for path in &files_to_delete {
                    match file_ops::delete_file(path) {
                        Ok(_) => success_count += 1,
                        Err(e) => last_error = e.to_string(),
                    }
                }
                let msg = if success_count == total {
                    Ok(format!("Deleted {} file(s)", success_count))
                } else {
                    Err(format!(
                        "Deleted {}/{}. Error: {}",
                        success_count, total, last_error
                    ))
                };
                let _ = tx.send(RemoteSpinnerResult::LocalOp {
                    message: msg,
                    reload: true,
                });
            });

            self.remote_spinner = Some(RemoteSpinner {
                message: "Deleting...".to_string(),
                started_at: Instant::now(),
                receiver: rx,
            });
        }
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
        let source_remote_profile = self
            .active_panel()
            .remote_ctx
            .as_ref()
            .map(|c| c.profile.clone());
        let count = files.len();

        self.clipboard = Some(Clipboard {
            files,
            source_path,
            operation: ClipboardOperation::Copy,
            source_remote_profile,
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
        let source_remote_profile = self
            .active_panel()
            .remote_ctx
            .as_ref()
            .map(|c| c.profile.clone());
        let count = files.len();

        self.clipboard = Some(Clipboard {
            files,
            source_path,
            operation: ClipboardOperation::Cut,
            source_remote_profile,
        });

        self.show_message(&format!("{} file(s) cut to clipboard", count));
    }

    /// Paste files from clipboard to current panel (Shift+V)
    pub fn clipboard_paste(&mut self) {
        let clipboard = match self.clipboard.take() {
            Some(cb) => cb,
            None => {
                self.show_message("Clipboard is empty");
                return;
            }
        };

        let source_is_remote = clipboard.source_remote_profile.is_some();
        let target_is_remote = self.active_panel().is_remote();
        let target_remote_profile = self
            .active_panel()
            .remote_ctx
            .as_ref()
            .map(|c| c.profile.clone());

        // Remote involved — use remote transfer path (no conflict detection for remote)
        if source_is_remote || target_is_remote {
            let is_cut = clipboard.operation == ClipboardOperation::Cut;
            let op_type = if is_cut {
                FileOperationType::Move
            } else {
                FileOperationType::Copy
            };

            // Remote-to-remote: download to local temp, then upload
            if source_is_remote && target_is_remote {
                let source_profile = match clipboard.source_remote_profile.clone() {
                    Some(p) => p,
                    None => {
                        self.clipboard = Some(clipboard);
                        self.show_message("Source remote profile not found");
                        return;
                    }
                };
                let target_profile = match target_remote_profile {
                    Some(p) => p,
                    None => {
                        self.clipboard = Some(clipboard);
                        self.show_message("Target remote profile not found");
                        return;
                    }
                };

                let target_path = self.active_panel().path.clone();
                let file_paths: Vec<PathBuf> = clipboard.files.iter().map(PathBuf::from).collect();
                let source_base = clipboard.source_path.display().to_string();
                let target = target_path.display().to_string();

                // Set pending focus to pasted file names
                if !clipboard.files.is_empty() {
                    self.pending_paste_focus = Some(clipboard.files.clone());
                }

                let mut progress = FileOperationProgress::new(op_type);
                progress.is_active = true;
                progress.total_files = file_paths.len();
                let cancel_flag = progress.cancel_flag.clone();
                let (tx, rx) = mpsc::channel();
                progress.receiver = Some(rx);

                thread::spawn(move || {
                    remote_transfer::transfer_remote_to_remote_with_progress(
                        source_profile,
                        target_profile,
                        file_paths,
                        source_base,
                        target,
                        cancel_flag,
                        tx,
                        is_cut,
                    );
                });

                self.file_operation_progress = Some(progress);
                self.dialog = Some(Dialog {
                    dialog_type: DialogType::Progress,
                    input: String::new(),
                    cursor_pos: 0,
                    message: String::new(),
                    completion: None,
                    selected_button: 0,
                    selection: None,
                    use_md5: false,
                });

                // Keep clipboard for copy, consume for cut
                if !is_cut {
                    self.clipboard = Some(clipboard);
                }
                return;
            }

            let profile = if source_is_remote {
                clipboard.source_remote_profile.clone()
            } else {
                target_remote_profile
            };

            let Some(profile) = profile else {
                self.clipboard = Some(clipboard);
                self.show_message("Remote profile not found");
                return;
            };

            let direction = if source_is_remote {
                remote_transfer::TransferDirection::RemoteToLocal
            } else {
                remote_transfer::TransferDirection::LocalToRemote
            };

            // For cut: determine source_profile for deletion
            // RemoteToLocal: source is remote → pass source remote profile
            // LocalToRemote: source is local → None
            let source_profile_for_delete = if is_cut && source_is_remote {
                clipboard.source_remote_profile.clone()
            } else {
                None
            };

            let target_path = self.active_panel().path.clone();
            let valid_files: Vec<String> = clipboard.files.clone();
            let file_paths: Vec<PathBuf> = valid_files.iter().map(PathBuf::from).collect();
            let source_base = clipboard.source_path.display().to_string();
            let target = target_path.display().to_string();

            // Set pending focus to pasted file names
            if !valid_files.is_empty() {
                self.pending_paste_focus = Some(valid_files.clone());
            }

            let mut progress = FileOperationProgress::new(op_type);
            progress.is_active = true;
            progress.total_files = file_paths.len();
            let cancel_flag = progress.cancel_flag.clone();
            let (tx, rx) = mpsc::channel();
            progress.receiver = Some(rx);

            let config = remote_transfer::TransferConfig {
                direction,
                profile,
                source_files: file_paths,
                source_base,
                target_path: target,
            };

            thread::spawn(move || {
                remote_transfer::transfer_files_with_progress(
                    config,
                    cancel_flag,
                    tx,
                    is_cut,
                    source_profile_for_delete,
                );
            });

            self.file_operation_progress = Some(progress);
            self.dialog = Some(Dialog {
                dialog_type: DialogType::Progress,
                input: String::new(),
                cursor_pos: 0,
                message: String::new(),
                completion: None,
                selected_button: 0,
                selection: None,
                use_md5: false,
            });

            // Keep clipboard for copy, consume for cut
            if !is_cut {
                self.clipboard = Some(clipboard);
            }
            return;
        }

        // Both local — existing local paste logic
        let target_path = self.active_panel().path.clone();

        // Check if source and target are the same (use canonical paths for robustness)
        let is_same_folder = match (
            clipboard.source_path.canonicalize().map(strip_unc_prefix),
            target_path.canonicalize().map(strip_unc_prefix),
        ) {
            (Ok(src), Ok(dest)) => src == dest,
            _ => clipboard.source_path == target_path, // Fallback to direct comparison
        };

        if is_same_folder {
            // For Cut operation in same folder, it doesn't make sense
            if clipboard.operation == ClipboardOperation::Cut {
                self.clipboard = Some(clipboard);
                self.show_message("Cannot move files to the same folder");
                return;
            }
            // For Copy operation in same folder, create duplicate with _dup suffix
            self.execute_same_folder_paste(clipboard);
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
        let canonical_target = target_path.canonicalize().map(strip_unc_prefix).ok();

        // Filter out files that would cause cycle
        let mut valid_files: Vec<String> = Vec::new();
        for file_name in &clipboard.files {
            let src = clipboard.source_path.join(file_name);

            // Check for copying/moving directory into itself
            if let (Some(ref target_canon), Ok(src_canon)) =
                (&canonical_target, src.canonicalize().map(strip_unc_prefix))
            {
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

        // Detect conflicts (files that already exist at destination)
        let conflicts = self.detect_paste_conflicts(&clipboard, &target_path, &valid_files);

        if !conflicts.is_empty() {
            // Has conflicts - show conflict dialog
            let is_move = clipboard.operation == ClipboardOperation::Cut;
            self.conflict_state = Some(ConflictState {
                conflicts,
                current_index: 0,
                files_to_overwrite: Vec::new(),
                files_to_skip: Vec::new(),
                valid_files,
                clipboard_backup: Some(clipboard),
                is_move_operation: is_move,
                target_path: target_path.clone(),
            });
            self.show_duplicate_conflict_dialog();
            return;
        }

        // No conflicts - proceed with normal paste
        self.execute_paste_operation(clipboard, valid_files, target_path);
    }

    /// Detect files that would conflict (already exist) at paste destination
    fn detect_paste_conflicts(
        &self,
        clipboard: &Clipboard,
        target_dir: &Path,
        valid_files: &[String],
    ) -> Vec<(PathBuf, PathBuf, String)> {
        let mut conflicts = Vec::new();

        for file_name in valid_files {
            let src = clipboard.source_path.join(file_name);
            let dest = target_dir.join(file_name);

            // Match file_ops: a broken symlink at dest still counts as existing,
            // so the user gets an overwrite prompt instead of a hard failure.
            if std::fs::symlink_metadata(&dest).is_ok() {
                conflicts.push((src, dest, file_name.clone()));
            }
        }

        conflicts
    }

    /// Generate a duplicate filename with _dup suffix, checking for existence
    /// e.g., "file.txt" -> "file_dup.txt", if exists -> "file_dup2.txt", etc.
    fn generate_dup_filename(name: &str, target_dir: &Path) -> String {
        let generate_name = |base: &str, ext: &str, suffix: &str| -> String {
            if ext.is_empty() {
                format!("{}{}", base, suffix)
            } else {
                format!("{}{}{}", base, suffix, ext)
            }
        };

        let (base, ext) = if let Some(dot_pos) = name.rfind('.') {
            let (b, e) = name.split_at(dot_pos);
            (b.to_string(), e.to_string())
        } else {
            (name.to_string(), String::new())
        };

        // Try _dup first (symlink_metadata: a broken symlink still occupies the name)
        let dup_name = generate_name(&base, &ext, "_dup");
        if std::fs::symlink_metadata(target_dir.join(&dup_name)).is_err() {
            return dup_name;
        }

        // If _dup exists, try _dup2, _dup3, etc.
        let mut counter = 2;
        loop {
            let suffix = format!("_dup{}", counter);
            let dup_name = generate_name(&base, &ext, &suffix);
            if std::fs::symlink_metadata(target_dir.join(&dup_name)).is_err() {
                return dup_name;
            }
            counter += 1;
            // Safety limit to prevent infinite loop
            if counter > 10000 {
                return generate_name(
                    &base,
                    &ext,
                    &format!(
                        "_dup{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0)
                    ),
                );
            }
        }
    }

    /// Show the duplicate conflict dialog
    pub fn show_duplicate_conflict_dialog(&mut self) {
        self.dialog = Some(Dialog {
            dialog_type: DialogType::DuplicateConflict,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    /// Execute paste operation (internal, called after conflict resolution or when no conflicts)
    fn execute_paste_operation(
        &mut self,
        clipboard: Clipboard,
        valid_files: Vec<String>,
        target_path: PathBuf,
    ) {
        // Set pending focus to pasted file names (will find first match in sorted file list)
        if !valid_files.is_empty() {
            self.pending_paste_focus = Some(valid_files.clone());
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
        thread::spawn(move || match clipboard_operation {
            ClipboardOperation::Copy => {
                file_ops::copy_files_with_progress(
                    file_paths,
                    &source_path,
                    &target_path,
                    HashSet::new(),
                    HashSet::new(),
                    cancel_flag,
                    tx,
                );
            }
            ClipboardOperation::Cut => {
                file_ops::move_files_with_progress(
                    file_paths,
                    &source_path,
                    &target_path,
                    HashSet::new(),
                    HashSet::new(),
                    cancel_flag,
                    tx,
                );
            }
        });

        // Store progress state and show dialog
        self.file_operation_progress = Some(progress);
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Progress,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });

        // Keep clipboard for copy operations (can paste multiple times)
        // Clear clipboard for cut operations (files are moved)
        if clipboard.operation == ClipboardOperation::Copy {
            self.clipboard = Some(clipboard);
        }
    }

    /// Execute paste operation for same folder (creates _dup copies)
    fn execute_same_folder_paste(&mut self, clipboard: Clipboard) {
        let source_path = clipboard.source_path.clone();

        // Filter valid files (skip ".." and non-existent)
        let valid_files: Vec<String> = clipboard
            .files
            .iter()
            .filter(|f| *f != ".." && source_path.join(f).exists())
            .cloned()
            .collect();

        if valid_files.is_empty() {
            self.clipboard = Some(clipboard);
            self.show_message("No valid files to duplicate");
            return;
        }

        // Create progress state
        let mut progress = FileOperationProgress::new(FileOperationType::Copy);
        progress.is_active = true;
        let cancel_flag = progress.cancel_flag.clone();

        // Create channel for progress messages
        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        // Build rename map: original name -> dup name
        let mut rename_map: Vec<(PathBuf, PathBuf)> = Vec::new();
        for file_name in &valid_files {
            let dup_name = Self::generate_dup_filename(file_name, &source_path);
            let src = source_path.join(file_name);
            let dest = source_path.join(&dup_name);
            rename_map.push((src, dest));
        }

        // Set pending focus to all dup file names (will find first match in sorted file list)
        let dup_names: Vec<String> = rename_map
            .iter()
            .filter_map(|(_, dest)| dest.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect();
        if !dup_names.is_empty() {
            self.pending_paste_focus = Some(dup_names);
        }

        // Start operation in background thread
        thread::spawn(move || {
            let mut completed = 0;
            let mut failed = 0;
            let source_paths: Vec<PathBuf> =
                rename_map.iter().map(|(src, _)| src.clone()).collect();

            let _ = tx.send(ProgressMessage::Preparing(
                "Calculating file sizes...".to_string(),
            ));
            let (total_bytes, total_files) =
                match file_ops::calculate_total_size(&source_paths, &cancel_flag) {
                    Ok((bytes, files)) => (bytes, files),
                    Err(e) => {
                        let message = if e.kind() == std::io::ErrorKind::Interrupted {
                            "Cancelled".to_string()
                        } else {
                            e.to_string()
                        };
                        let failure_count = if message == "Cancelled" {
                            1
                        } else {
                            rename_map.len()
                        };
                        let _ = tx.send(ProgressMessage::Error(String::new(), message));
                        let _ = tx.send(ProgressMessage::Completed(0, failure_count));
                        return;
                    }
                };

            let _ = tx.send(ProgressMessage::PrepareComplete);
            let _ = tx.send(ProgressMessage::TotalProgress(
                0,
                total_files,
                0,
                total_bytes,
            ));

            let mut completed_bytes: u64 = 0;
            let mut completed_files: usize = 0;

            for (src, dest) in rename_map {
                if cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = tx.send(ProgressMessage::Error(
                        String::new(),
                        "Cancelled".to_string(),
                    ));
                    let _ = tx.send(ProgressMessage::Completed(completed, failed + 1));
                    return;
                }

                let file_name = src
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();

                // Safety check: never overwrite existing files
                // (symlink_metadata: a broken symlink at dest still counts as existing)
                if std::fs::symlink_metadata(&dest).is_ok() {
                    let _ = tx.send(ProgressMessage::Error(
                        file_name.clone(),
                        "destination already exists".to_string(),
                    ));
                    failed += 1;
                    continue;
                }

                let _ = tx.send(ProgressMessage::FileStarted(file_name.clone()));

                let result = if src.is_dir() {
                    file_ops::copy_dir_recursive_with_progress(
                        &src,
                        &dest,
                        &cancel_flag,
                        &tx,
                        &mut completed_bytes,
                        &mut completed_files,
                        total_bytes,
                        total_files,
                    )
                } else {
                    let file_size = std::fs::metadata(&src).map(|m| m.len()).unwrap_or(0);
                    let file_completed_bytes = completed_bytes;
                    file_ops::copy_file_with_progress(&src, &dest, &cancel_flag, |copied, total| {
                        let _ = tx.send(ProgressMessage::FileProgress(copied, total));
                        let _ = tx.send(ProgressMessage::TotalProgress(
                            completed_files,
                            total_files,
                            file_completed_bytes + copied,
                            total_bytes,
                        ));
                    })
                    .map(|_| {
                        completed_bytes += file_size;
                        completed_files += 1;
                    })
                };

                match result {
                    Ok(_) => {
                        completed += 1;
                        let _ = tx.send(ProgressMessage::FileCompleted(file_name));
                    }
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::Interrupted {
                            if dest.is_dir() {
                                let _ = std::fs::remove_dir_all(&dest);
                            } else {
                                let _ = std::fs::remove_file(&dest);
                            }
                            let _ = tx.send(ProgressMessage::Error(
                                String::new(),
                                "Cancelled".to_string(),
                            ));
                            let _ = tx.send(ProgressMessage::Completed(completed, failed + 1));
                            return;
                        }
                        failed += 1;
                        let _ = tx.send(ProgressMessage::Error(file_name, e.to_string()));
                    }
                }
            }

            let _ = tx.send(ProgressMessage::Completed(completed, failed));
        });

        // Store progress state and show dialog
        self.file_operation_progress = Some(progress);
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Progress,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });

        // Keep clipboard for copy operations
        self.clipboard = Some(clipboard);
    }

    /// Execute paste operation with conflict resolution (overwrite/skip sets)
    pub fn execute_paste_with_conflicts(&mut self) {
        let conflict_state = match self.conflict_state.take() {
            Some(state) => state,
            None => return,
        };

        let clipboard = match conflict_state.clipboard_backup {
            Some(cb) => cb,
            None => return,
        };

        let target_path = conflict_state.target_path;

        // Build all files to process from the pre-conflict validated list.
        let valid_files = conflict_state.valid_files;

        // Build overwrite and skip sets from source paths
        let files_to_overwrite: HashSet<PathBuf> =
            conflict_state.files_to_overwrite.into_iter().collect();
        let files_to_skip: HashSet<PathBuf> = conflict_state.files_to_skip.into_iter().collect();

        // Check if all files would be skipped
        let files_to_process: Vec<&String> = valid_files
            .iter()
            .filter(|f| {
                let src = clipboard.source_path.join(f);
                !files_to_skip.contains(&src)
            })
            .collect();

        // Set pending focus to all non-skipped file names (will find first match in sorted file list)
        if !files_to_process.is_empty() {
            self.pending_paste_focus =
                Some(files_to_process.iter().map(|f| (*f).clone()).collect());
        }

        if files_to_process.is_empty() {
            // All files were skipped - show message and restore clipboard if copy
            if clipboard.operation == ClipboardOperation::Copy {
                self.clipboard = Some(clipboard);
            }
            self.show_message("All files skipped");
            self.refresh_panels();
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
        thread::spawn(move || match clipboard_operation {
            ClipboardOperation::Copy => {
                file_ops::copy_files_with_progress(
                    file_paths,
                    &source_path,
                    &target_path,
                    files_to_overwrite,
                    files_to_skip,
                    cancel_flag,
                    tx,
                );
            }
            ClipboardOperation::Cut => {
                file_ops::move_files_with_progress(
                    file_paths,
                    &source_path,
                    &target_path,
                    files_to_overwrite,
                    files_to_skip,
                    cancel_flag,
                    tx,
                );
            }
        });

        // Store progress state and show dialog
        self.file_operation_progress = Some(progress);
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Progress,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
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
            self.image_viewer_state = Some(crate::ui::image_viewer::ImageViewerState::new(&path));
            self.current_screen = Screen::ImageViewer;
        }
    }

    pub fn execute_open_large_file(&mut self) {
        if let Some(path) = self.pending_large_file.take() {
            let mut viewer = ViewerState::new();
            viewer.set_syntax_colors(self.theme.syntax);
            match viewer.load_file(&path) {
                Ok(_) => {
                    self.viewer_state = Some(viewer);
                    self.current_screen = Screen::FileViewer;
                }
                Err(e) => {
                    self.show_message(&format!("Cannot read file: {}", e));
                }
            }
        }
    }

    pub fn execute_mkdir(&mut self, name: &str) {
        // Validate filename to prevent path traversal attacks
        if let Err(e) = file_ops::is_valid_filename(name) {
            self.show_message(&format!("Error: {}", e));
            return;
        }

        if self.active_panel().is_remote() {
            // Remote mkdir via SFTP (async with spinner)
            if self.remote_spinner.is_some() {
                return;
            }
            let panel_idx = self.active_panel_index;
            let mut ctx = match self.panels[panel_idx].remote_ctx.take() {
                Some(ctx) => ctx,
                None => return,
            };
            let remote_base = self.active_panel().path.display().to_string();
            let remote_path = format!("{}/{}", remote_base.trim_end_matches('/'), name);
            let focus_name = name.to_string();
            let display_name = name.to_string();
            let (tx, rx) = mpsc::channel();

            thread::spawn(move || {
                let msg = match ctx.session.mkdir(&remote_path) {
                    Ok(_) => Ok(format!("Created directory: {}", display_name)),
                    Err(e) => Err(e),
                };
                let _ = tx.send(RemoteSpinnerResult::PanelOp {
                    ctx,
                    panel_idx,
                    outcome: PanelOpOutcome::Simple {
                        message: msg,
                        pending_focus: Some(focus_name),
                        reload: true,
                    },
                });
            });

            self.remote_spinner = Some(RemoteSpinner {
                message: "Creating directory...".to_string(),
                started_at: Instant::now(),
                receiver: rx,
            });
            return;
        }

        let path = self.active_panel().path.join(name);

        // Additional check: ensure the resulting path is within the current directory
        if let Ok(canonical_parent) = self
            .active_panel()
            .path
            .canonicalize()
            .map(strip_unc_prefix)
        {
            if let Ok(canonical_new) = path.canonicalize().map(strip_unc_prefix).or_else(|_| {
                // For new directories, check the parent path
                path.parent()
                    .and_then(|p| p.canonicalize().map(strip_unc_prefix).ok())
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
            Ok(_) => {
                self.active_panel_mut().pending_focus = Some(name.to_string());
                self.show_message(&format!("Created directory: {}", name));
            }
            Err(e) => self.show_message(&format!("Error: {}", e)),
        }
        self.refresh_panels();
    }

    pub fn execute_mkfile(&mut self, name: &str) {
        // Validate filename to prevent path traversal attacks
        if let Err(e) = file_ops::is_valid_filename(name) {
            self.show_message(&format!("Error: {}", e));
            return;
        }

        if self.active_panel().is_remote() {
            // Remote file creation via SFTP (async with spinner)
            if self.remote_spinner.is_some() {
                return;
            }
            let panel_idx = self.active_panel_index;
            let mut ctx = match self.panels[panel_idx].remote_ctx.take() {
                Some(ctx) => ctx,
                None => return,
            };
            let remote_base = self.active_panel().path.display().to_string();
            let remote_path = format!("{}/{}", remote_base.trim_end_matches('/'), name);
            let focus_name = name.to_string();
            let display_name = name.to_string();
            let (tx, rx) = mpsc::channel();

            thread::spawn(move || {
                let msg = match ctx.session.create_file(&remote_path) {
                    Ok(_) => Ok(format!("Created file: {}", display_name)),
                    Err(e) => Err(e),
                };
                let _ = tx.send(RemoteSpinnerResult::PanelOp {
                    ctx,
                    panel_idx,
                    outcome: PanelOpOutcome::Simple {
                        message: msg,
                        pending_focus: Some(focus_name),
                        reload: true,
                    },
                });
            });

            self.remote_spinner = Some(RemoteSpinner {
                message: "Creating file...".to_string(),
                started_at: Instant::now(),
                receiver: rx,
            });
            return;
        }

        let path = self.active_panel().path.join(name);

        // Check if file already exists
        if path.exists() {
            self.show_message(&format!("'{}' already exists!", name));
            return;
        }

        // Create empty file
        match std::fs::File::create(&path) {
            Ok(_) => {
                self.active_panel_mut().pending_focus = Some(name.to_string());
                self.refresh_panels();

                // Open the file in editor
                let mut editor = EditorState::new();
                editor.set_syntax_colors(self.theme.syntax);
                match editor.load_file(&path) {
                    Ok(_) => {
                        self.editor_state = Some(editor);
                        self.current_screen = Screen::FileEditor;
                    }
                    Err(e) => {
                        self.show_message(&format!("File created but cannot open: {}", e));
                    }
                }
            }
            Err(e) => self.show_message(&format!("Error: {}", e)),
        }
    }

    pub fn execute_rename(&mut self, new_name: &str) {
        // Validate filename to prevent path traversal attacks
        if let Err(e) = file_ops::is_valid_filename(new_name) {
            self.show_message(&format!("Error: {}", e));
            return;
        }

        if let Some(file) = self.active_panel().current_file() {
            let old_name = file.name.clone();

            if self.active_panel().is_remote() {
                // Remote rename via SFTP (async with spinner)
                if self.remote_spinner.is_some() {
                    return;
                }
                let panel_idx = self.active_panel_index;
                let mut ctx = match self.panels[panel_idx].remote_ctx.take() {
                    Some(ctx) => ctx,
                    None => return,
                };
                let remote_base = self.active_panel().path.display().to_string();
                let old_remote = format!("{}/{}", remote_base.trim_end_matches('/'), old_name);
                let new_remote = format!("{}/{}", remote_base.trim_end_matches('/'), new_name);
                let focus_name = new_name.to_string();
                let display_name = new_name.to_string();
                let (tx, rx) = mpsc::channel();

                thread::spawn(move || {
                    let msg = match ctx.session.rename(&old_remote, &new_remote) {
                        Ok(_) => Ok(format!("Renamed to: {}", display_name)),
                        Err(e) => Err(e),
                    };
                    let _ = tx.send(RemoteSpinnerResult::PanelOp {
                        ctx,
                        panel_idx,
                        outcome: PanelOpOutcome::Simple {
                            message: msg,
                            pending_focus: Some(focus_name),
                            reload: true,
                        },
                    });
                });

                self.remote_spinner = Some(RemoteSpinner {
                    message: "Renaming...".to_string(),
                    started_at: Instant::now(),
                    receiver: rx,
                });
                return;
            }

            let old_path = self.active_panel().path.join(&old_name);
            let new_path = self.active_panel().path.join(new_name);

            // Additional check: ensure the new path stays within the current directory
            if let Ok(canonical_parent) = self
                .active_panel()
                .path
                .canonicalize()
                .map(strip_unc_prefix)
            {
                // For rename, we verify against parent directory
                if let Some(new_parent) = new_path.parent() {
                    if let Ok(canonical_new_parent) =
                        new_parent.canonicalize().map(strip_unc_prefix)
                    {
                        if canonical_new_parent != canonical_parent {
                            self.show_message("Error: Path traversal attempt detected");
                            return;
                        }
                    }
                }
            }

            match file_ops::rename_file(&old_path, &new_path) {
                Ok(_) => {
                    self.active_panel_mut().pending_focus = Some(new_name.to_string());
                    self.show_message(&format!("Renamed to: {}", new_name));
                }
                Err(e) => self.show_message(&format!("Error: {}", e)),
            }
            self.refresh_panels();
        }
    }

    pub fn execute_tar(&mut self, archive_name: &str) {
        if self.active_panel().is_remote() {
            self.show_message("Archive creation is not supported on remote panels");
            return;
        }
        // Fast validations only (no I/O or external processes)
        if let Err(e) = file_ops::is_valid_filename(archive_name) {
            self.show_message(&format!("Error: {}", e));
            return;
        }

        let files = self.get_operation_files();
        if files.is_empty() {
            self.show_message("No files to archive");
            return;
        }

        // Validate each filename to prevent argument injection
        for file in &files {
            if let Err(e) = file_ops::is_valid_filename(file) {
                self.show_message(&format!("Invalid filename '{}': {}", file, e));
                return;
            }
        }

        let current_dir = self.active_panel().path.clone();
        let archive_path = current_dir.join(archive_name);

        // Check if archive already exists (fast check)
        if archive_path.exists() {
            self.show_message(&format!("Error: {} already exists", archive_name));
            return;
        }

        // Check for unsafe symlinks BEFORE starting background work
        let (_, excluded_paths) = file_ops::filter_symlinks_for_tar(&current_dir, &files);

        // If there are files to exclude, show confirmation dialog
        if !excluded_paths.is_empty() {
            self.tar_exclude_state = Some(TarExcludeState {
                archive_name: archive_name.to_string(),
                files: files.clone(),
                excluded_paths,
                scroll_offset: 0,
            });
            self.dialog = Some(Dialog {
                dialog_type: DialogType::TarExcludeConfirm,
                input: String::new(),
                cursor_pos: 0,
                message: String::new(),
                completion: None,
                selected_button: 0,
                selection: None,
                use_md5: false,
            });
            return;
        }

        // No exclusions needed - proceed directly
        self.execute_tar_with_excludes(archive_name, &files, &[]);
    }

    /// Execute tar with specified exclusions (called after confirmation or when no exclusions needed)
    pub fn execute_tar_with_excludes(
        &mut self,
        archive_name: &str,
        files: &[String],
        excluded_paths: &[String],
    ) {
        use std::io::BufReader;
        use std::process::{Command, Stdio};

        let current_dir = self.active_panel().path.clone();

        // Determine compression option based on extension
        let tar_options = if archive_name.ends_with(".tar.gz") || archive_name.ends_with(".tgz") {
            "cvfpz"
        } else if archive_name.ends_with(".tar.bz2") || archive_name.ends_with(".tbz2") {
            "cvfpj"
        } else if archive_name.ends_with(".tar.xz") || archive_name.ends_with(".txz") {
            "cvfpJ"
        } else {
            "cvfp"
        };

        let tar_options_owned = tar_options.to_string();
        let archive_name_owned = archive_name.to_string();
        let archive_path_clone = current_dir.join(archive_name);
        let files_owned = files.to_vec();
        let excluded_owned = excluded_paths.to_vec();

        // Create progress state with preparing flag - show dialog immediately
        let mut progress = FileOperationProgress::new(FileOperationType::Tar);
        progress.is_active = true;
        progress.is_preparing = true;
        progress.preparing_message = "Preparing...".to_string();
        let cancel_flag = progress.cancel_flag.clone();

        // Create channel for progress messages
        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        // Clear selection before starting
        self.active_panel_mut().selected_files.clear();

        // Store progress state and show dialog IMMEDIATELY
        self.file_operation_progress = Some(progress);
        self.pending_tar_archive = Some(archive_name.to_string());
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Progress,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });

        // Clone tar_path from settings for use in background thread
        let tar_path = self.settings.tar_path.clone();

        // Start all preparation and execution in background thread
        thread::spawn(move || {
            // Check for cancellation
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    archive_name_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Build tar_args with --exclude options for unsafe symlinks
            // Note: archive name must come right after options (e.g., cvfpz archive.tar.gz)
            let mut tar_args = vec![tar_options_owned.clone(), archive_name_owned.clone()];
            for excluded in &excluded_owned {
                tar_args.push(format!("--exclude=./{}", excluded));
            }
            tar_args.extend(files_owned.iter().map(|f| format!("./{}", f)));

            // Check for cancellation
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    archive_name_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Determine tar command (in background)
            let _ = tx.send(ProgressMessage::Preparing(
                "Checking tar command...".to_string(),
            ));
            let tar_cmd = if let Some(ref custom_tar) = tar_path {
                // Use custom tar path from settings
                match Command::new(custom_tar).arg("--version").output() {
                    Ok(output) if output.status.success() => Some(custom_tar.clone()),
                    _ => None,
                }
            } else {
                // Default: try gtar first, then tar
                match Command::new("gtar").arg("--version").output() {
                    Ok(output) if output.status.success() => Some("gtar".to_string()),
                    _ => match Command::new("tar").arg("--version").output() {
                        Ok(output) if output.status.success() => Some("tar".to_string()),
                        _ => None,
                    },
                }
            };

            let tar_cmd = match tar_cmd {
                Some(cmd) => cmd,
                None => {
                    let _ = tx.send(ProgressMessage::Error(
                        archive_name_owned,
                        "tar command not found".to_string(),
                    ));
                    let _ = tx.send(ProgressMessage::Completed(0, 1));
                    return;
                }
            };

            // Check if stdbuf is available (in background)
            let has_stdbuf = Command::new("stdbuf")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

            // Check for cancellation
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    archive_name_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Calculate file sizes
            let _ = tx.send(ProgressMessage::Preparing(
                "Calculating file sizes...".to_string(),
            ));

            // Check for cancellation during preparation
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    archive_name_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Calculate total size and file size map (in background)
            let (total_bytes, size_map) = Self::calculate_tar_sizes(&current_dir, &files_owned);
            let total_file_count = size_map.len();

            // Check for cancellation after preparation
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    archive_name_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Preparation complete, send initial totals
            let _ = tx.send(ProgressMessage::PrepareComplete);
            let _ = tx.send(ProgressMessage::TotalProgress(
                0,
                total_file_count,
                0,
                total_bytes,
            ));

            // Helper function to cleanup partial archive
            let cleanup_archive = |path: &PathBuf| {
                let _ = std::fs::remove_file(path);
            };

            // Use stdbuf to disable buffering if available. Each child is
            // placed into its own process group so kill_child_tree's
            // group-targeted SIGKILL stays scoped to tar (and never the
            // cokacdir TUI process itself).
            let child = if has_stdbuf {
                let mut args = vec!["-o0".to_string(), "-e0".to_string(), tar_cmd.clone()];
                args.extend(tar_args);
                let mut cmd = Command::new("stdbuf");
                cmd.current_dir(&current_dir)
                    .args(&args)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                crate::services::claude::detach_into_own_pgroup(&mut cmd);
                cmd.spawn()
            } else {
                let mut cmd = Command::new(&tar_cmd);
                cmd.current_dir(&current_dir)
                    .args(&tar_args)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                crate::services::claude::detach_into_own_pgroup(&mut cmd);
                cmd.spawn()
            };

            match child {
                Ok(mut child) => {
                    let (cancel_watch_done, cancel_watch) =
                        spawn_process_cancel_watchdog(cancel_flag.clone(), child.id());
                    let stdout = child.stdout.take();
                    let stderr = child.stderr.take();
                    let mut completed_files = 0usize;
                    let mut completed_bytes = 0u64;
                    let mut stdout_error_lines: Vec<String> = Vec::new();

                    // Collect stderr in background for error messages
                    let stderr_handle = stderr.map(|stderr| {
                        thread::spawn(move || {
                            use std::io::Read;
                            let mut err_str = String::new();
                            let mut stderr = stderr;
                            let _ = stderr.read_to_string(&mut err_str);
                            err_str
                        })
                    });

                    // Read stdout line by line for progress updates
                    // (tar outputs verbose listing to stdout on most systems)
                    if let Some(stdout) = stdout {
                        use std::io::BufRead;
                        let mut reader = BufReader::with_capacity(64, stdout);
                        let mut line = String::new();

                        loop {
                            // Check for cancellation
                            if cancel_flag.load(Ordering::Relaxed) {
                                crate::services::claude::kill_child_tree(&mut child);
                                let _ = child.wait();
                                cancel_watch_done.store(true, Ordering::Relaxed);
                                let _ = cancel_watch.join();
                                // Cleanup partial archive on cancellation
                                cleanup_archive(&archive_path_clone);
                                let _ = tx.send(ProgressMessage::Error(
                                    archive_name_owned.clone(),
                                    "Cancelled".to_string(),
                                ));
                                let _ = tx.send(ProgressMessage::Completed(completed_files, 1));
                                return;
                            }

                            line.clear();
                            match reader.read_line(&mut line) {
                                Ok(0) => break, // EOF
                                Ok(_) => {
                                    let filename = line.trim_end();
                                    // Check if this looks like an error line (starts with "tar:")
                                    if filename.starts_with("tar:") || filename.starts_with("gtar:")
                                    {
                                        stdout_error_lines.push(filename.to_string());
                                    } else if !filename.is_empty() {
                                        completed_files += 1;
                                        // Look up file size from the map
                                        if let Some(&file_size) = size_map.get(filename) {
                                            completed_bytes += file_size;
                                        }
                                        let _ = tx.send(ProgressMessage::FileStarted(
                                            filename.to_string(),
                                        ));
                                        let _ = tx.send(ProgressMessage::FileCompleted(
                                            filename.to_string(),
                                        ));
                                        let _ = tx.send(ProgressMessage::TotalProgress(
                                            completed_files,
                                            total_file_count,
                                            completed_bytes,
                                            total_bytes,
                                        ));
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    }

                    // Wait for completion
                    let wait_result = child.wait();
                    cancel_watch_done.store(true, Ordering::Relaxed);
                    let _ = cancel_watch.join();
                    match wait_result {
                        Ok(status) => {
                            if cancel_flag.load(Ordering::Relaxed) {
                                cleanup_archive(&archive_path_clone);
                                let _ = tx.send(ProgressMessage::Error(
                                    archive_name_owned.clone(),
                                    "Cancelled".to_string(),
                                ));
                                let _ = tx.send(ProgressMessage::Completed(completed_files, 1));
                                return;
                            }
                            if status.success() {
                                let _ = tx.send(ProgressMessage::Completed(completed_files, 0));
                            } else {
                                // Cleanup partial archive on failure
                                cleanup_archive(&archive_path_clone);
                                let error_msg = Self::process_error_message(
                                    stderr_handle.and_then(|h| h.join().ok()),
                                    &stdout_error_lines,
                                    "tar command failed",
                                );
                                let _ =
                                    tx.send(ProgressMessage::Error(archive_name_owned, error_msg));
                                let _ = tx.send(ProgressMessage::Completed(0, 1));
                            }
                        }
                        Err(e) => {
                            // Cleanup partial archive on error
                            cleanup_archive(&archive_path_clone);
                            let _ =
                                tx.send(ProgressMessage::Error(archive_name_owned, e.to_string()));
                            let _ = tx.send(ProgressMessage::Completed(0, 1));
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(ProgressMessage::Error(
                        archive_name_owned,
                        format!("Failed to run tar: {}", e),
                    ));
                    let _ = tx.send(ProgressMessage::Completed(0, 1));
                }
            }
        });
    }

    /// List archive contents to get total file count and sizes
    fn list_archive_contents(
        tar_cmd: &str,
        archive_path: &std::path::Path,
        archive_name: &str,
        cancel_flag: Arc<AtomicBool>,
    ) -> Result<(usize, u64, std::collections::HashMap<String, u64>), String> {
        use std::collections::HashMap;
        use std::process::{Command, Stdio};

        // Determine list option based on extension
        let list_options = if archive_name.ends_with(".tar.gz") || archive_name.ends_with(".tgz") {
            "tvfz"
        } else if archive_name.ends_with(".tar.bz2") || archive_name.ends_with(".tbz2") {
            "tvfj"
        } else if archive_name.ends_with(".tar.xz") || archive_name.ends_with(".txz") {
            "tvfJ"
        } else {
            "tvf"
        };

        let mut total_files = 0usize;
        let mut total_bytes = 0u64;
        let mut size_map = HashMap::new();

        let archive_path_str = archive_path.to_string_lossy().to_string();
        let mut cmd = Command::new(tar_cmd);
        cmd.arg(list_options)
            .arg(&archive_path_str)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::services::claude::detach_into_own_pgroup(&mut cmd);

        let output = match cmd.spawn() {
            Ok(child) => {
                let (cancel_watch_done, cancel_watch) =
                    spawn_process_cancel_watchdog(cancel_flag.clone(), child.id());
                let output = child.wait_with_output();
                cancel_watch_done.store(true, Ordering::Relaxed);
                let _ = cancel_watch.join();
                output
            }
            Err(e) => return Err(format!("Failed to list archive contents: {}", e)),
        };

        if cancel_flag.load(Ordering::Relaxed) {
            return Err("Cancelled".to_string());
        }

        let output = output.map_err(|e| format!("Failed to list archive contents: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stdout_error_lines: Vec<String> = stdout
                .lines()
                .map(|line| line.trim_end_matches('\r'))
                .filter(|line| line.starts_with("tar:") || line.starts_with("gtar:"))
                .map(ToString::to_string)
                .collect();

            return Err(Self::process_error_message(
                Some(stderr),
                &stdout_error_lines,
                "Failed to read archive contents",
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            // tar -tvf output format: -rw-r--r-- user/group    1234 2024-01-01 12:00 filename
            // Parse the line to extract size and filename
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 6 {
                // Size is typically the 3rd field (index 2)
                if let Ok(size) = parts[2].parse::<u64>() {
                    // Filename is everything after the date/time (index 5+)
                    let filename = parts[5..].join(" ");
                    size_map.insert(filename, size);
                    total_bytes += size;
                }
                total_files += 1;
            }
        }

        Ok((total_files, total_bytes, size_map))
    }

    /// Execute archive extraction with progress display
    pub fn execute_untar(&mut self, archive_path: &std::path::Path) {
        if self.active_panel().is_remote() {
            self.show_message("Archive extraction is not supported on remote panels");
            return;
        }
        use std::io::BufReader;
        use std::process::{Command, Stdio};

        let archive_name = match archive_path.file_name() {
            Some(name) => name.to_string_lossy().to_string(),
            None => {
                self.show_message("Invalid archive path");
                return;
            }
        };

        // Fast validations only
        if !archive_path.exists() {
            self.show_message(&format!("Archive not found: {}", archive_name));
            return;
        }

        let current_dir = match archive_path.parent() {
            Some(dir) => dir.to_path_buf(),
            None => {
                self.show_message("Invalid archive path");
                return;
            }
        };

        // Determine extraction directory name (remove archive extensions)
        let extract_dir_name = archive_name
            .trim_end_matches(".tar.gz")
            .trim_end_matches(".tgz")
            .trim_end_matches(".tar.bz2")
            .trim_end_matches(".tbz2")
            .trim_end_matches(".tar.xz")
            .trim_end_matches(".txz")
            .trim_end_matches(".tar")
            .to_string();

        let extract_path = current_dir.join(&extract_dir_name);

        // Check if extraction directory already exists (fast check)
        if extract_path.exists() {
            self.show_message(&format!("Error: {} already exists", extract_dir_name));
            return;
        }

        // Determine decompression option based on extension
        let tar_options = if archive_name.ends_with(".tar.gz") || archive_name.ends_with(".tgz") {
            "xvfpz"
        } else if archive_name.ends_with(".tar.bz2") || archive_name.ends_with(".tbz2") {
            "xvfpj"
        } else if archive_name.ends_with(".tar.xz") || archive_name.ends_with(".txz") {
            "xvfpJ"
        } else {
            "xvfp"
        };

        let archive_path_owned = archive_path.to_path_buf();
        let archive_name_owned = archive_name.clone();
        let extract_dir_owned = extract_dir_name.clone();
        let extract_path_clone = extract_path.clone();

        // Create progress state with preparing flag - show dialog immediately
        let mut progress = FileOperationProgress::new(FileOperationType::Untar);
        progress.is_active = true;
        progress.is_preparing = true;
        progress.preparing_message = "Preparing...".to_string();
        let cancel_flag = progress.cancel_flag.clone();

        // Create channel for progress messages
        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        // Store progress state and show dialog IMMEDIATELY
        self.file_operation_progress = Some(progress);
        self.pending_extract_dir = Some(extract_dir_name);
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Progress,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });

        // Clone tar_path from settings for use in background thread
        let tar_path = self.settings.tar_path.clone();

        // Start all preparation and execution in background thread
        thread::spawn(move || {
            // Check for cancellation
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    extract_dir_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Determine tar command (in background)
            let _ = tx.send(ProgressMessage::Preparing(
                "Checking tar command...".to_string(),
            ));
            let tar_cmd = if let Some(ref custom_tar) = tar_path {
                // Use custom tar path from settings
                match Command::new(custom_tar).arg("--version").output() {
                    Ok(output) if output.status.success() => Some(custom_tar.clone()),
                    _ => None,
                }
            } else {
                // Default: try gtar first, then tar
                match Command::new("gtar").arg("--version").output() {
                    Ok(output) if output.status.success() => Some("gtar".to_string()),
                    _ => match Command::new("tar").arg("--version").output() {
                        Ok(output) if output.status.success() => Some("tar".to_string()),
                        _ => None,
                    },
                }
            };

            let tar_cmd = match tar_cmd {
                Some(cmd) => cmd,
                None => {
                    let _ = tx.send(ProgressMessage::Error(
                        extract_dir_owned,
                        "tar command not found".to_string(),
                    ));
                    let _ = tx.send(ProgressMessage::Completed(0, 1));
                    return;
                }
            };

            // Check if stdbuf is available (in background)
            let has_stdbuf = Command::new("stdbuf")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

            // Check for cancellation
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    extract_dir_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // List archive contents
            let _ = tx.send(ProgressMessage::Preparing(
                "Reading archive contents...".to_string(),
            ));
            let (total_file_count, total_bytes, size_map) = match Self::list_archive_contents(
                &tar_cmd,
                &archive_path_owned,
                &archive_name_owned,
                cancel_flag.clone(),
            ) {
                Ok(contents) => contents,
                Err(e) => {
                    let _ = tx.send(ProgressMessage::Error(extract_dir_owned, e));
                    let _ = tx.send(ProgressMessage::Completed(0, 1));
                    return;
                }
            };

            // Check for cancellation after listing
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    extract_dir_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            if total_file_count == 0 {
                let _ = tx.send(ProgressMessage::Error(
                    extract_dir_owned,
                    "Archive appears to be empty or corrupted".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Create extraction directory
            if let Err(e) = std::fs::create_dir(&extract_path_clone) {
                let _ = tx.send(ProgressMessage::Error(
                    extract_dir_owned,
                    format!("Failed to create directory: {}", e),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Preparation complete, send initial totals
            let _ = tx.send(ProgressMessage::PrepareComplete);
            let _ = tx.send(ProgressMessage::TotalProgress(
                0,
                total_file_count,
                0,
                total_bytes,
            ));

            // Build command arguments
            let archive_path_str = archive_path_owned.to_string_lossy().to_string();
            let tar_args = vec![tar_options.to_string(), archive_path_str];

            // Execute tar extraction. Each child is placed into its own
            // process group so kill_child_tree's group-targeted SIGKILL
            // stays scoped to tar (and never the cokacdir TUI process itself).
            let child = if has_stdbuf {
                let mut args = vec!["-oL".to_string(), "-eL".to_string(), tar_cmd.clone()];
                args.extend(tar_args);
                let mut cmd = Command::new("stdbuf");
                cmd.current_dir(&extract_path_clone)
                    .args(&args)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                crate::services::claude::detach_into_own_pgroup(&mut cmd);
                cmd.spawn()
            } else {
                let mut cmd = Command::new(&tar_cmd);
                cmd.current_dir(&extract_path_clone)
                    .args(&tar_args)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                crate::services::claude::detach_into_own_pgroup(&mut cmd);
                cmd.spawn()
            };

            // Cleanup helper for failed extraction
            let cleanup_extract_dir = |path: &std::path::PathBuf| {
                let _ = std::fs::remove_dir_all(path);
            };

            match child {
                Ok(mut child) => {
                    let (cancel_watch_done, cancel_watch) =
                        spawn_process_cancel_watchdog(cancel_flag.clone(), child.id());
                    let stdout = child.stdout.take();
                    let stderr = child.stderr.take();
                    let mut completed_files = 0usize;
                    let mut completed_bytes = 0u64;
                    let mut stdout_error_lines: Vec<String> = Vec::new();

                    // Collect stderr in background for error messages
                    let stderr_handle = stderr.map(|stderr| {
                        thread::spawn(move || {
                            use std::io::Read;
                            let mut err_str = String::new();
                            let mut stderr = stderr;
                            let _ = stderr.read_to_string(&mut err_str);
                            err_str
                        })
                    });

                    // Read stdout line by line for progress updates
                    if let Some(stdout) = stdout {
                        use std::io::BufRead;
                        let mut reader = BufReader::with_capacity(256, stdout);
                        let mut line = String::new();

                        loop {
                            // Check for cancellation
                            if cancel_flag.load(Ordering::Relaxed) {
                                crate::services::claude::kill_child_tree(&mut child);
                                let _ = child.wait();
                                cancel_watch_done.store(true, Ordering::Relaxed);
                                let _ = cancel_watch.join();
                                cleanup_extract_dir(&extract_path_clone);
                                let _ = tx.send(ProgressMessage::Error(
                                    extract_dir_owned.clone(),
                                    "Cancelled".to_string(),
                                ));
                                let _ = tx.send(ProgressMessage::Completed(completed_files, 1));
                                return;
                            }

                            line.clear();
                            match reader.read_line(&mut line) {
                                Ok(0) => break, // EOF
                                Ok(_) => {
                                    let filename = line.trim_end();
                                    if filename.starts_with("tar:") || filename.starts_with("gtar:")
                                    {
                                        stdout_error_lines.push(filename.to_string());
                                    } else if !filename.is_empty() {
                                        completed_files += 1;
                                        // Look up file size from the map
                                        if let Some(&file_size) = size_map.get(filename) {
                                            completed_bytes += file_size;
                                        }
                                        let _ = tx.send(ProgressMessage::FileStarted(
                                            filename.to_string(),
                                        ));
                                        let _ = tx.send(ProgressMessage::FileCompleted(
                                            filename.to_string(),
                                        ));
                                        let _ = tx.send(ProgressMessage::TotalProgress(
                                            completed_files,
                                            total_file_count,
                                            completed_bytes,
                                            total_bytes,
                                        ));
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    }

                    // Wait for completion
                    let wait_result = child.wait();
                    cancel_watch_done.store(true, Ordering::Relaxed);
                    let _ = cancel_watch.join();
                    match wait_result {
                        Ok(status) => {
                            if cancel_flag.load(Ordering::Relaxed) {
                                cleanup_extract_dir(&extract_path_clone);
                                let _ = tx.send(ProgressMessage::Error(
                                    extract_dir_owned.clone(),
                                    "Cancelled".to_string(),
                                ));
                                let _ = tx.send(ProgressMessage::Completed(completed_files, 1));
                                return;
                            }
                            if status.success() {
                                let _ = tx.send(ProgressMessage::Completed(completed_files, 0));
                            } else {
                                cleanup_extract_dir(&extract_path_clone);
                                let error_msg = Self::process_error_message(
                                    stderr_handle.and_then(|h| h.join().ok()),
                                    &stdout_error_lines,
                                    "tar extraction failed",
                                );
                                let _ =
                                    tx.send(ProgressMessage::Error(extract_dir_owned, error_msg));
                                let _ = tx.send(ProgressMessage::Completed(0, 1));
                            }
                        }
                        Err(e) => {
                            cleanup_extract_dir(&extract_path_clone);
                            let _ =
                                tx.send(ProgressMessage::Error(extract_dir_owned, e.to_string()));
                            let _ = tx.send(ProgressMessage::Completed(0, 1));
                        }
                    }
                }
                Err(e) => {
                    cleanup_extract_dir(&extract_path_clone);
                    let _ = tx.send(ProgressMessage::Error(
                        extract_dir_owned,
                        format!("Failed to run tar: {}", e),
                    ));
                    let _ = tx.send(ProgressMessage::Completed(0, 1));
                }
            }
        });
    }

    pub fn execute_search(&mut self, term: &str) {
        if self.active_panel().is_remote() {
            self.show_message("Search is not supported on remote panels");
            return;
        }
        if term.trim().is_empty() {
            self.show_message("Please enter a search term");
            return;
        }
        if self.remote_spinner.is_some() {
            return;
        }

        let base_path = self.active_panel().path.clone();
        let search_term = term.to_string();
        let base_path_clone = base_path.clone();
        let term_clone = search_term.clone();
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let results = crate::ui::search_result::execute_recursive_search(
                &base_path_clone,
                &term_clone,
                1000,
            );
            let _ = tx.send(RemoteSpinnerResult::SearchComplete {
                results,
                search_term: term_clone,
                base_path: base_path_clone,
            });
        });

        self.remote_spinner = Some(RemoteSpinner {
            message: "Searching...".to_string(),
            started_at: Instant::now(),
            receiver: rx,
        });
    }

    pub fn execute_goto(&mut self, path_str: &str) {
        // Check if this is a remote path (user@host:/path)
        if let Some((user, host, port, remote_path)) = remote::parse_remote_path(path_str) {
            self.execute_goto_remote(&user, &host, port, &remote_path);
            return;
        }

        // If the current panel is remote:
        // - ~ should disconnect and go local home
        // - ~/subdir should disconnect and go local ~/subdir
        // - /absolute/path: if exists locally → disconnect and go local, otherwise remote navigation
        // - Relative paths are remote navigation
        if self.active_panel().is_remote() {
            if self.remote_spinner.is_some() {
                // Don't disconnect while a background operation is using remote_ctx
                return;
            }
            if path_str == "~" {
                // Just go to local home - disconnect handles navigation
                self.disconnect_remote_panel();
                return;
            } else if path_str.starts_with("~/") || path_str.starts_with("~\\") {
                // Disconnect and fall through to local goto for ~/subdir
                self.disconnect_remote_panel();
            } else if path_str.starts_with('/')
                || (cfg!(windows) && PathBuf::from(path_str).is_absolute())
            {
                // Absolute path: check if it exists on the local filesystem
                let local_path = PathBuf::from(path_str);
                if local_path.exists() {
                    // Path exists locally → disconnect from remote and navigate locally
                    self.disconnect_remote_panel();
                    // fall through to local goto
                } else {
                    // Not a local path → navigate within remote
                    self.execute_goto_remote_relative(path_str);
                    return;
                }
            } else {
                // Relative path → navigate within remote
                self.execute_goto_remote_relative(path_str);
                return;
            }
        }

        // Security: Check for path traversal attempts
        if path_str.contains("..") {
            // Normalize the path to resolve .. components
            let normalized = if path_str.starts_with('~') {
                dirs::home_dir()
                    .map(|h| h.join(path_str[1..].trim_start_matches(['/', '\\'])))
                    .unwrap_or_else(|| PathBuf::from(path_str))
            } else if PathBuf::from(path_str).is_absolute() {
                PathBuf::from(path_str)
            } else {
                self.active_panel().path.join(path_str)
            };

            // Canonicalize to resolve all .. components
            match normalized.canonicalize().map(strip_unc_prefix) {
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
                .map(|h| h.join(path_str[1..].trim_start_matches(['/', '\\'])))
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

        if valid_path == path && valid_path == fallback {
            // 이미 해당 경로에 있음
            self.show_message(&format!("Already at: {}", valid_path.display()));
        } else if valid_path != fallback {
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

    /// Handle goto for remote path (user@host:/path)
    fn execute_goto_remote(&mut self, user: &str, host: &str, port: u16, remote_path: &str) {
        // Check if we have a matching saved profile
        if let Some(profile) =
            remote::find_matching_profile(&self.settings.remote_profiles, user, host, port)
        {
            // Use saved profile credentials to connect
            let profile = profile.clone();
            let path = if remote_path == "/" && !profile.default_path.is_empty() {
                profile.default_path.clone()
            } else {
                remote_path.to_string()
            };
            self.connect_remote_panel(&profile, &path);
        } else {
            // No saved profile — show remote connect dialog for auth
            let state = RemoteConnectState::from_parsed(user, host, port, remote_path);
            self.remote_connect_state = Some(state);
            self.dialog = Some(Dialog {
                dialog_type: DialogType::RemoteConnect,
                input: String::new(),
                cursor_pos: 0,
                message: format!("Connect to {}@{}:{}", user, host, port),
                completion: None,
                selected_button: 0,
                selection: None,
                use_md5: false,
            });
        }
    }

    /// Handle goto for relative path on a remote panel (async with spinner)
    fn execute_goto_remote_relative(&mut self, path_str: &str) {
        if self.remote_spinner.is_some() {
            return;
        }

        let current = self.active_panel().path.display().to_string();
        let new_path = if path_str == ".." {
            // Go to parent directory
            if current == "/" {
                return;
            }
            let parent = std::path::Path::new(&current)
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "/".to_string());
            parent
        } else if path_str.starts_with('/') {
            path_str.to_string()
        } else {
            format!("{}/{}", current.trim_end_matches('/'), path_str)
        };

        self.spawn_remote_list_dir(&new_path);
    }

    /// Connect a panel to a remote server (async with spinner)
    pub fn connect_remote_panel(&mut self, profile: &remote::RemoteProfile, path: &str) {
        if self.remote_spinner.is_some() {
            return;
        }

        let (tx, rx) = mpsc::channel();
        let profile_clone = profile.clone();
        let path_clone = path.to_string();
        let panel_idx = self.active_panel_index;

        thread::spawn(move || {
            let result = match remote::SftpSession::connect(&profile_clone) {
                Ok(session) => {
                    let mut ctx = RemoteContext {
                        profile: profile_clone.clone(),
                        session,
                        status: ConnectionStatus::Connected,
                    };
                    // Try listing the requested path
                    match ctx.session.list_dir(&path_clone) {
                        Ok(entries) => Ok(ConnectSuccess {
                            ctx: Box::new(ctx),
                            entries,
                            path: path_clone,
                            fallback_msg: None,
                            profile: profile_clone,
                        }),
                        Err(_) => {
                            // Fallback to /
                            match ctx.session.list_dir("/") {
                                Ok(entries) => Ok(ConnectSuccess {
                                    ctx: Box::new(ctx),
                                    entries,
                                    path: "/".to_string(),
                                    fallback_msg: Some(format!(
                                        "Path not found: {} — moved to /",
                                        path_clone
                                    )),
                                    profile: profile_clone,
                                }),
                                Err(e2) => Err(format!("Connection failed: {}", e2)),
                            }
                        }
                    }
                }
                Err(e) => Err(format!("Connection failed: {}", e)),
            };
            let _ = tx.send(RemoteSpinnerResult::Connected { result, panel_idx });
        });

        self.remote_spinner = Some(RemoteSpinner {
            message: format!("Connecting to {}@{}...", profile.user, profile.host),
            started_at: Instant::now(),
            receiver: rx,
        });
    }

    /// Disconnect remote panel and switch back to local
    pub fn disconnect_remote_panel(&mut self) {
        let panel = self.active_panel_mut();
        if let Some(mut ctx) = panel.remote_ctx.take() {
            ctx.session.disconnect();
        }
        panel.remote_display = None;
        let home = dirs::home_dir().unwrap_or_else(|| {
            if cfg!(windows) {
                PathBuf::from("C:\\")
            } else {
                PathBuf::from("/")
            }
        });
        panel.path = home;
        panel.selected_index = 0;
        panel.selected_files.clear();
        panel.load_files();
        self.show_message("Disconnected from remote server");
    }

    /// Spawn a background thread for remote list_dir operation
    fn spawn_remote_list_dir(&mut self, new_path: &str) {
        if self.remote_spinner.is_some() {
            return;
        }
        let panel_idx = self.active_panel_index;
        let mut ctx = match self.panels[panel_idx].remote_ctx.take() {
            Some(ctx) => ctx,
            None => return,
        };
        // Save old path for rollback on failure
        let old_path = self.panels[panel_idx].path.clone();
        // Update panel path now so header shows the new remote path during loading
        self.panels[panel_idx].path = PathBuf::from(new_path);
        let path = new_path.to_string();
        let path_for_result = PathBuf::from(new_path);
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let entries = ctx.session.list_dir(&path);
            let _ = tx.send(RemoteSpinnerResult::PanelOp {
                ctx,
                panel_idx,
                outcome: PanelOpOutcome::ListDir {
                    entries,
                    path: path_for_result,
                    old_path: Some(old_path),
                },
            });
        });

        self.remote_spinner = Some(RemoteSpinner {
            message: "Loading...".to_string(),
            started_at: Instant::now(),
            receiver: rx,
        });
    }

    /// Spawn a background thread for remote list_dir (for panel refresh)
    pub fn spawn_remote_refresh(&mut self, panel_idx: usize) {
        if self.remote_spinner.is_some() {
            return;
        }
        let mut ctx = match self.panels[panel_idx].remote_ctx.take() {
            Some(ctx) => ctx,
            None => return,
        };
        let path = self.panels[panel_idx].path.display().to_string();
        let path_for_result = self.panels[panel_idx].path.clone();
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let entries = ctx.session.list_dir(&path);
            let _ = tx.send(RemoteSpinnerResult::PanelOp {
                ctx,
                panel_idx,
                outcome: PanelOpOutcome::ListDir {
                    entries,
                    path: path_for_result,
                    old_path: None,
                },
            });
        });

        self.remote_spinner = Some(RemoteSpinner {
            message: "Loading...".to_string(),
            started_at: Instant::now(),
            receiver: rx,
        });
    }

    fn start_pending_remote_editor_upload(&mut self) -> bool {
        if self.remote_spinner.is_some() {
            return false;
        }

        let (panel_idx, remote_path, local_path, generation) = {
            let Some(editor) = self.editor_state.as_mut() else {
                return false;
            };
            if !editor.remote_dirty {
                return false;
            }
            let Some(origin) = editor.remote_origin.as_ref() else {
                return false;
            };
            if origin.panel_index >= self.panels.len() {
                editor.set_message("Saved locally, remote panel is no longer available", 50);
                return false;
            }
            let is_connected = self.panels[origin.panel_index]
                .remote_ctx
                .as_ref()
                .map(|ctx| matches!(ctx.status, ConnectionStatus::Connected))
                .unwrap_or(false);
            if !is_connected {
                editor.set_message("Saved locally, remote connection is not available", 50);
                return false;
            }
            (
                origin.panel_index,
                origin.remote_path.clone(),
                editor.file_path.display().to_string(),
                editor.remote_save_generation,
            )
        };

        let ctx = match self.panels[panel_idx].remote_ctx.take() {
            Some(ctx) => ctx,
            None => {
                if let Some(editor) = self.editor_state.as_mut() {
                    editor.set_message("Saved locally, remote connection was disconnected", 50);
                }
                return false;
            }
        };
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let msg = match ctx.session.upload_file(&local_path, &remote_path) {
                Ok(_) => Ok("Saved & uploaded to remote!".to_string()),
                Err(e) => Err(format!("Saved locally, upload failed: {}", e)),
            };
            let _ = tx.send(RemoteSpinnerResult::PanelOp {
                ctx,
                panel_idx,
                outcome: PanelOpOutcome::RemoteSave {
                    message: msg,
                    remote_path,
                    generation,
                    reload: true,
                },
            });
        });

        self.remote_spinner = Some(RemoteSpinner {
            message: "Uploading...".to_string(),
            started_at: Instant::now(),
            receiver: rx,
        });
        if let Some(editor) = self.editor_state.as_mut() {
            editor.set_message("Uploading latest local save...", 30);
        }
        true
    }

    /// Poll the remote spinner for completion
    pub fn poll_remote_spinner(&mut self) {
        let result = if let Some(ref spinner) = self.remote_spinner {
            match spinner.receiver.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(())),
            }
        } else {
            return;
        };

        let result = match result {
            Some(Ok(r)) => r,
            Some(Err(())) => {
                // Thread panicked or sender dropped — cancel spinner
                self.remote_spinner = None;
                self.show_message("Remote operation failed unexpectedly");
                return;
            }
            None => return,
        };

        // Spinner completed — remove it
        self.remote_spinner = None;
        let mut retry_pending_remote_save = true;

        match result {
            RemoteSpinnerResult::Connected { result, panel_idx } => {
                match result {
                    Ok(success) => {
                        let panel = &mut self.panels[panel_idx];
                        panel.remote_display = Some((
                            success.ctx.profile.user.clone(),
                            success.ctx.profile.host.clone(),
                            success.ctx.profile.port,
                        ));
                        panel.remote_ctx = Some(success.ctx);
                        panel.selected_index = 0;
                        panel.selected_files.clear();
                        // Update connection status
                        if let Some(ref mut ctx) = panel.remote_ctx {
                            ctx.status = ConnectionStatus::Connected;
                        }
                        panel.apply_remote_entries(success.entries, &PathBuf::from(&success.path));

                        // Auto-save profile and bookmark on first connection to this server
                        let already_has_profile = self.settings.remote_profiles.iter().any(|p| {
                            p.user == success.profile.user
                                && p.host == success.profile.host
                                && p.port == success.profile.port
                        });
                        let already_bookmarked = self.settings.bookmarked_path.iter().any(|bm| {
                            if let Some((bu, bh, bp, _)) = remote::parse_remote_path(bm) {
                                bu == success.profile.user
                                    && bh == success.profile.host
                                    && bp == success.profile.port
                            } else {
                                false
                            }
                        });
                        let mut settings_changed = false;
                        if !already_has_profile {
                            self.settings.remote_profiles.push(success.profile.clone());
                            settings_changed = true;
                        }
                        if !already_bookmarked {
                            let bookmark_path =
                                remote::format_remote_display(&success.profile, &success.path);
                            self.settings.bookmarked_path.push(bookmark_path);
                            settings_changed = true;
                        }
                        if settings_changed {
                            let _ = self.settings.save();
                        }

                        if let Some(msg) = success.fallback_msg {
                            self.show_extension_handler_error(&msg);
                        } else {
                            self.show_message(&format!(
                                "Connected to {}@{}",
                                success.profile.user, success.profile.host
                            ));
                        }
                    }
                    Err(e) => {
                        self.show_message(&e);
                    }
                }
            }
            RemoteSpinnerResult::PanelOp {
                ctx,
                panel_idx,
                outcome,
            } => {
                // Return ctx to panel
                self.panels[panel_idx].remote_ctx = Some(ctx);

                match outcome {
                    PanelOpOutcome::Simple {
                        message,
                        pending_focus,
                        reload,
                    } => {
                        let (msg_text, is_err) = match &message {
                            Ok(msg) => (msg.clone(), false),
                            Err(e) => (format!("Error: {}", e), true),
                        };
                        if !is_err {
                            if let Some(focus) = pending_focus {
                                self.panels[panel_idx].pending_focus = Some(focus);
                            }
                        }
                        // If in editor, set editor message; otherwise show app message
                        if self.current_screen == Screen::FileEditor {
                            if let Some(ref mut editor) = self.editor_state {
                                let duration = if is_err { 50 } else { 30 };
                                editor.set_message(msg_text, duration);
                            }
                        } else {
                            self.show_message(&msg_text);
                        }
                        if reload {
                            // Refresh local panels synchronously
                            for i in 0..self.panels.len() {
                                if !self.panels[i].is_remote() {
                                    self.panels[i].selected_files.clear();
                                    self.panels[i].load_files();
                                }
                            }
                            // For the remote panel, spawn another list_dir
                            if self.panels[panel_idx].is_remote() {
                                self.spawn_remote_refresh(panel_idx);
                            }
                        }
                    }
                    PanelOpOutcome::RemoteSave {
                        message,
                        remote_path,
                        generation,
                        reload,
                    } => {
                        let (mut msg_text, is_err) = match &message {
                            Ok(msg) => (msg.clone(), false),
                            Err(e) => (format!("Error: {}", e), true),
                        };
                        let mut current_upload_failed = false;
                        let mut editor_message_duration = if is_err { 50 } else { 30 };
                        if let Some(ref mut editor) = self.editor_state {
                            let is_current = editor.apply_remote_save_result(
                                panel_idx,
                                &remote_path,
                                generation,
                                is_err,
                            );
                            current_upload_failed = is_current && is_err;
                            if !is_current && editor.remote_dirty {
                                msg_text = if is_err {
                                    "Previous upload failed; latest local save still needs upload"
                                        .to_string()
                                } else {
                                    "Previous upload finished; latest local save still needs upload"
                                        .to_string()
                                };
                            }
                            editor_message_duration = if is_err || editor.remote_dirty {
                                50
                            } else {
                                30
                            };
                        }
                        if self.current_screen == Screen::FileEditor {
                            if let Some(ref mut editor) = self.editor_state {
                                editor.set_message(msg_text, editor_message_duration);
                            }
                        } else {
                            self.show_message(&msg_text);
                        }
                        if current_upload_failed {
                            retry_pending_remote_save = false;
                        }
                        if reload && !is_err {
                            // Refresh local panels synchronously
                            for i in 0..self.panels.len() {
                                if !self.panels[i].is_remote() {
                                    self.panels[i].selected_files.clear();
                                    self.panels[i].load_files();
                                }
                            }
                            // For the remote panel, spawn another list_dir
                            if self.panels[panel_idx].is_remote() {
                                self.spawn_remote_refresh(panel_idx);
                            }
                        }
                    }
                    PanelOpOutcome::ListDir {
                        entries,
                        path,
                        old_path,
                    } => {
                        match entries {
                            Ok(sftp_entries) => {
                                let panel = &mut self.panels[panel_idx];
                                panel.selected_index = 0;
                                panel.selected_files.clear();
                                if let Some(ref mut ctx) = panel.remote_ctx {
                                    ctx.status = ConnectionStatus::Connected;
                                }
                                panel.apply_remote_entries(sftp_entries, &path);
                            }
                            Err(e) => {
                                // Rollback path on failure
                                if let Some(prev) = old_path {
                                    self.panels[panel_idx].path = prev;
                                }
                                if let Some(ref mut ctx) = self.panels[panel_idx].remote_ctx {
                                    ctx.status = ConnectionStatus::Disconnected(e.clone());
                                }
                                self.show_message(&format!("Error: {}", e));
                            }
                        }
                    }
                    PanelOpOutcome::DirExists {
                        exists,
                        target_entry,
                    } => {
                        if exists {
                            self.execute_goto(&target_entry);
                        } else {
                            self.show_extension_handler_error(&format!(
                                "Path not found: {}",
                                target_entry
                            ));
                        }
                    }
                }
            }
            RemoteSpinnerResult::LocalOp { message, reload } => {
                match &message {
                    Ok(msg) => self.show_message(msg),
                    Err(e) => self.show_message(e),
                }
                if reload {
                    self.refresh_panels();
                }
            }
            RemoteSpinnerResult::SearchComplete {
                results,
                search_term,
                base_path,
            } => {
                if results.is_empty() {
                    self.show_message(&format!("No files found matching \"{}\"", search_term));
                } else {
                    self.search_result_state.results = results;
                    self.search_result_state.selected_index = 0;
                    self.search_result_state.scroll_offset = 0;
                    self.search_result_state.search_term = search_term;
                    self.search_result_state.base_path = base_path;
                    self.search_result_state.active = true;
                    self.current_screen = Screen::SearchResult;
                }
            }
            RemoteSpinnerResult::GitDiffComplete { result } => match result {
                Ok((dir1, dir2)) => {
                    self.enter_diff_screen(dir1, dir2);
                }
                Err(e) => {
                    self.show_message(&e);
                }
            },
        }

        if retry_pending_remote_save {
            self.start_pending_remote_editor_upload();
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
                    self.goto_directory_with_focus(parent, Some(item.name.clone()));
                }
            }
            // 검색 결과 화면 닫기
            self.search_result_state.active = false;
            self.current_screen = Screen::FilePanel;
            self.show_message(&format!("Moved to: {}", item.relative_path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

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
        let fallback = std::env::temp_dir();

        let result = get_valid_path(&temp_dir, &fallback);
        assert_eq!(result, temp_dir);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_get_valid_path_nonexistent_uses_parent() {
        let temp_dir = create_temp_dir();
        let nonexistent = temp_dir.join("does_not_exist");
        let fallback = std::env::temp_dir();

        let result = get_valid_path(&nonexistent, &fallback);
        assert_eq!(result, temp_dir);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_get_valid_path_fallback() {
        let nonexistent = PathBuf::from("/nonexistent/path/that/does/not/exist");
        let fallback = std::env::temp_dir();

        let result = get_valid_path(&nonexistent, &fallback);
        assert!(result.exists());
    }

    #[test]
    fn test_get_valid_path_root() {
        let root = if cfg!(windows) {
            PathBuf::from("C:\\")
        } else {
            PathBuf::from("/")
        };
        let fallback = std::env::temp_dir();

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
        let first_path = temp_dir.join("first");
        let second_path = temp_dir.join("second");

        fs::create_dir_all(&first_path).unwrap();
        fs::create_dir_all(&second_path).unwrap();

        let app = App::new(first_path.clone(), second_path.clone());

        assert_eq!(app.panels[0].path, first_path);
        assert_eq!(app.panels[1].path, second_path);
        assert_eq!(app.active_panel_index, 0);
        assert_eq!(app.current_screen, Screen::FilePanel);
        assert!(app.dialog.is_none());
        assert!(app.message.is_none());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_show_tar_dialog_defaults_to_tar_archive() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file.txt"), "content").unwrap();
        let mut app = App::new(temp_dir.clone(), temp_dir.clone());
        app.active_panel_mut()
            .selected_files
            .insert("file.txt".to_string());

        app.show_tar_dialog();

        let dialog = app.dialog.as_ref().unwrap();
        assert_eq!(dialog.dialog_type, DialogType::Tar);
        assert_eq!(dialog.input, "file.txt.tar");

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_app_switch_panel() {
        let temp_dir = create_temp_dir();
        fs::create_dir_all(temp_dir.join("panel1")).unwrap();
        fs::create_dir_all(temp_dir.join("panel2")).unwrap();

        let mut app = App::new(temp_dir.join("panel1"), temp_dir.join("panel2"));

        assert_eq!(app.active_panel_index, 0);

        app.switch_panel();
        assert_eq!(app.active_panel_index, 1);

        app.switch_panel();
        assert_eq!(app.active_panel_index, 0);

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
    fn test_execute_open_large_file_uses_viewer() {
        let temp_dir = create_temp_dir();
        let path = temp_dir.join("large.txt");
        fs::write(&path, "content").unwrap();
        let mut app = App::new(temp_dir.clone(), temp_dir.clone());
        app.pending_large_file = Some(path.clone());

        app.execute_open_large_file();

        assert_eq!(app.current_screen, Screen::FileViewer);
        assert!(app.viewer_state.is_some());
        assert!(app.editor_state.is_none());
        assert_eq!(app.viewer_state.as_ref().unwrap().file_path, path);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_file_operation_disconnected_cancel_reports_cancelled() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut progress = FileOperationProgress::new(FileOperationType::Copy);
        progress.is_active = true;
        progress.receiver = Some(rx);
        progress.cancel();
        drop(tx);

        assert!(!progress.poll());

        let result = progress.result.unwrap();
        assert_eq!(result.failure_count, 1);
        assert_eq!(result.last_error.as_deref(), Some("Cancelled"));
    }

    #[test]
    fn test_file_operation_disconnected_without_completion_reports_protocol_error() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut progress = FileOperationProgress::new(FileOperationType::Copy);
        progress.is_active = true;
        progress.receiver = Some(rx);
        tx.send(ProgressMessage::TotalProgress(3, 5, 123, 456))
            .unwrap();
        drop(tx);

        assert!(!progress.poll());

        let result = progress.result.unwrap();
        assert_eq!(result.success_count, 0);
        assert_eq!(result.failure_count, 1);
        assert_eq!(
            result.last_error.as_deref(),
            Some("Operation worker exited without a completion message")
        );
    }

    #[test]
    fn test_process_error_message_preserves_all_available_lines() {
        let stdout_errors = vec![
            "tar: first stdout error".to_string(),
            "tar: second stdout error".to_string(),
        ];

        let message = App::process_error_message(
            Some("stderr line 1\nstderr line 2\n".to_string()),
            &stdout_errors,
            "fallback",
        );

        assert_eq!(
            message,
            "stderr line 1\nstderr line 2\ntar: first stdout error\ntar: second stdout error"
        );
        assert_eq!(
            App::process_error_message(Some("\n".to_string()), &[], "fallback"),
            "fallback"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_list_archive_contents_reports_tar_stderr_on_failure() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = create_temp_dir();
        let fake_tar = temp_dir.join("fake_tar");
        let archive = temp_dir.join("bad.tar");

        fs::write(
            &fake_tar,
            "#!/bin/sh\necho 'tar: bad archive' >&2\necho 'tar: missing end marker' >&2\nexit 2\n",
        )
        .unwrap();
        fs::set_permissions(&fake_tar, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&archive, "not a tar archive").unwrap();

        let err = App::list_archive_contents(
            fake_tar.to_str().unwrap(),
            &archive,
            "bad.tar",
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();

        assert!(err.contains("tar: bad archive"));
        assert!(err.contains("tar: missing end marker"));

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
    fn test_panel_index_equality() {
        let idx_a: usize = 0;
        let idx_b: usize = 1;
        assert_eq!(idx_a, 0);
        assert_eq!(idx_b, 1);
        assert_ne!(idx_a, idx_b);
    }

    #[test]
    fn test_sort_by_equality() {
        assert_eq!(SortBy::Name, SortBy::Name);
        assert_eq!(SortBy::Size, SortBy::Size);
        assert_eq!(SortBy::Modified, SortBy::Modified);
    }

    #[test]
    fn test_screen_equality() {
        assert_eq!(Screen::FilePanel, Screen::FilePanel);
        assert_eq!(Screen::FileViewer, Screen::FileViewer);
        assert_ne!(Screen::FilePanel, Screen::Help);
    }

    #[test]
    fn test_dialog_type_equality() {
        assert_eq!(DialogType::Delete, DialogType::Delete);
        assert_ne!(DialogType::Delete, DialogType::Mkdir);
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
        while app
            .file_operation_progress
            .as_ref()
            .map(|p| p.is_active)
            .unwrap_or(false)
        {
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
        while app
            .file_operation_progress
            .as_ref()
            .map(|p| p.is_active)
            .unwrap_or(false)
        {
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
    fn test_clipboard_paste_same_folder_creates_duplicate() {
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

        // Wait for async duplicate operation to complete
        while app
            .file_operation_progress
            .as_ref()
            .map(|p| p.is_active)
            .unwrap_or(false)
        {
            if let Some(ref mut progress) = app.file_operation_progress {
                progress.poll();
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(temp_dir.join("file.txt").exists());
        assert!(temp_dir.join("file_dup.txt").exists());
        assert_eq!(
            fs::read_to_string(temp_dir.join("file_dup.txt")).unwrap(),
            "content"
        );

        // Clipboard should still exist (copy can be pasted multiple times)
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
