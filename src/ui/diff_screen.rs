use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use chrono::{DateTime, Local};
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use super::app::{App, Dialog, DialogType, FileOperationProgress, Screen, SortBy, SortOrder};
use super::theme::Theme;
use crate::services::file_ops::{self, FileOperationType, ProgressMessage};
use crate::utils::format::{format_size, safe_suffix, truncate_with_ellipsis};

// ═══════════════════════════════════════════════════════════════════════════════
// Data structures
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffStatus {
    Same,
    Modified,
    LeftOnly,
    RightOnly,
    DirModified,
    DirSame,
}

#[derive(Debug, Clone)]
pub struct DiffFileInfo {
    pub name: String,
    pub size: u64,
    pub modified: DateTime<Local>,
    pub is_directory: bool,
    pub is_symlink: bool,
    pub full_path: PathBuf,
    authorization: Option<file_ops::PathAuthorization>,
}

#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub relative_path: String,
    pub left: Option<DiffFileInfo>,
    pub right: Option<DiffFileInfo>,
    pub status: DiffStatus,
    pub is_directory: bool,
    pub depth: usize,
    /// true if this is a one-side-only directory whose children have not been loaded yet
    pub children_not_loaded: bool,
    left_missing: Option<file_ops::MissingPathAuthorization>,
    right_missing: Option<file_ops::MissingPathAuthorization>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffFilter {
    All,
    DifferentOnly,
    LeftOnly,
    RightOnly,
}

impl DiffFilter {
    pub fn next(&self) -> DiffFilter {
        match self {
            DiffFilter::All => DiffFilter::DifferentOnly,
            DiffFilter::DifferentOnly => DiffFilter::LeftOnly,
            DiffFilter::LeftOnly => DiffFilter::RightOnly,
            DiffFilter::RightOnly => DiffFilter::All,
        }
    }

    pub fn display_name(&self) -> &str {
        match self {
            DiffFilter::All => "All",
            DiffFilter::DifferentOnly => "Different Only",
            DiffFilter::LeftOnly => "Left Only",
            DiffFilter::RightOnly => "Right Only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareMethod {
    Content,
    ModifiedTime,
    ContentAndTime,
}

impl Default for CompareMethod {
    fn default() -> Self {
        CompareMethod::Content
    }
}

impl CompareMethod {
    pub fn display_name(&self) -> &str {
        match self {
            CompareMethod::Content => "Content",
            CompareMethod::ModifiedTime => "Modified Time",
            CompareMethod::ContentAndTime => "Content + Time",
        }
    }
}

/// Parse compare method from string (for CLI argument parsing)
pub fn parse_compare_method(s: &str) -> CompareMethod {
    match s.to_lowercase().as_str() {
        "content" => CompareMethod::Content,
        "time" | "modified" | "modifiedtime" | "modified_time" => CompareMethod::ModifiedTime,
        "contentandtime" | "content_and_time" | "contenttime" => CompareMethod::ContentAndTime,
        _ => CompareMethod::default(),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Async diff types
// ═══════════════════════════════════════════════════════════════════════════════

struct DiffCompareResult(Vec<DiffEntry>);

enum DiffProgressMsg {
    Counting(usize),
    Comparing(String, usize, usize),
}

struct DiffCursorAnchor {
    focused_path: Option<String>,
    following_paths: Vec<String>,
    preceding_paths: Vec<String>,
    parent_path: Option<String>,
    visual_row: usize,
    previous_index: usize,
    previous_scroll: usize,
}

#[derive(Default)]
struct CreatedAncestorSides {
    left: Vec<String>,
    right: Vec<String>,
}

#[derive(Debug, Clone)]
struct DiffAuthorizedItem {
    parent: file_ops::DirectoryAuthorization,
    item: file_ops::PathAuthorization,
    tree: Option<file_ops::TreeAuthorization>,
}

#[derive(Debug, Clone)]
struct DiffCopyPrompt {
    relative_path: PathBuf,
    left_root: file_ops::DirectoryAuthorization,
    right_root: file_ops::DirectoryAuthorization,
    left_item: Option<DiffAuthorizedItem>,
    right_item: Option<DiffAuthorizedItem>,
    left_missing: Option<file_ops::MissingPathAuthorization>,
    right_missing: Option<file_ops::MissingPathAuthorization>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffCopyDirection {
    ToLeft,
    ToRight,
}

#[derive(Debug, Clone)]
struct DiffDeletePrompt {
    relative_path: PathBuf,
    left_root: file_ops::DirectoryAuthorization,
    right_root: file_ops::DirectoryAuthorization,
    left_item: Option<DiffAuthorizedItem>,
    right_item: Option<DiffAuthorizedItem>,
    left_missing: Option<file_ops::MissingPathAuthorization>,
    right_missing: Option<file_ops::MissingPathAuthorization>,
    contains_directory: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffDeleteDirection {
    Left,
    Both,
    Right,
}

#[derive(Debug, Clone)]
struct PreparedDiffDeleteTarget {
    side: &'static str,
    root: file_ops::DirectoryAuthorization,
    parent: file_ops::DirectoryAuthorization,
    path: PathBuf,
    item: file_ops::PathAuthorization,
    tree: Option<file_ops::TreeAuthorization>,
}

// ═══════════════════════════════════════════════════════════════════════════════
// DiffState
// ═══════════════════════════════════════════════════════════════════════════════

pub struct DiffState {
    pub left_root: PathBuf,
    pub right_root: PathBuf,
    pub all_entries: Vec<DiffEntry>,
    pub filtered_indices: Vec<usize>,
    pub selected_index: usize,
    pub scroll_offset: usize,
    pub filter: DiffFilter,
    pub sort_by: SortBy,
    pub sort_order: SortOrder,
    pub compare_method: CompareMethod,
    pub selected_files: HashSet<String>,
    pub visible_height: usize,
    /// Set of relative_path values for collapsed directories
    pub collapsed_dirs: HashSet<String>,
    // Async comparison fields
    pub is_comparing: bool,
    cancel_flag: Arc<AtomicBool>,
    receiver: Option<Receiver<DiffCompareResult>>,
    progress_receiver: Option<Receiver<DiffProgressMsg>>,
    pub progress_current: String,
    pub progress_count: usize,
    pub progress_total: usize,
    left_root_authorization: Option<file_ops::DirectoryAuthorization>,
    right_root_authorization: Option<file_ops::DirectoryAuthorization>,
    copy_prompt: Option<DiffCopyPrompt>,
    copy_in_progress: bool,
    pending_copy_path: Option<String>,
    delete_prompt: Option<DiffDeletePrompt>,
    delete_in_progress: bool,
    pending_delete_path: Option<String>,
}

impl DiffState {
    pub fn new(
        left: PathBuf,
        right: PathBuf,
        compare_method: CompareMethod,
        sort_by: SortBy,
        sort_order: SortOrder,
    ) -> Self {
        Self {
            left_root: left,
            right_root: right,
            all_entries: Vec::new(),
            filtered_indices: Vec::new(),
            selected_index: 0,
            scroll_offset: 0,
            filter: DiffFilter::DifferentOnly,
            sort_by,
            sort_order,
            compare_method,
            selected_files: HashSet::new(),
            visible_height: 0,
            collapsed_dirs: HashSet::new(),
            is_comparing: false,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            receiver: None,
            progress_receiver: None,
            progress_current: String::new(),
            progress_count: 0,
            progress_total: 0,
            left_root_authorization: None,
            right_root_authorization: None,
            copy_prompt: None,
            copy_in_progress: false,
            pending_copy_path: None,
            delete_prompt: None,
            delete_in_progress: false,
            pending_delete_path: None,
        }
    }

    /// Start async comparison in a background thread
    pub fn start_comparison(&mut self) {
        // Cancel any previous comparison
        self.cancel_flag.store(true, Ordering::Relaxed);

        self.is_comparing = true;
        self.all_entries.clear();
        self.filtered_indices.clear();
        self.collapsed_dirs.clear();
        self.selected_index = 0;
        self.scroll_offset = 0;
        self.progress_current = String::new();
        self.progress_count = 0;
        self.progress_total = 0;
        self.left_root_authorization =
            file_ops::capture_directory_authorization(&self.left_root).ok();
        self.right_root_authorization =
            file_ops::capture_directory_authorization(&self.right_root).ok();
        self.copy_prompt = None;
        self.copy_in_progress = false;
        self.pending_copy_path = None;
        self.delete_prompt = None;
        self.delete_in_progress = false;
        self.pending_delete_path = None;
        self.cancel_flag = Arc::new(AtomicBool::new(false));

        let (result_tx, result_rx) = mpsc::channel();
        let (progress_tx, progress_rx) = mpsc::channel();
        self.receiver = Some(result_rx);
        self.progress_receiver = Some(progress_rx);

        let left_root = self.left_root.clone();
        let right_root = self.right_root.clone();
        let left_root_authorization = self.left_root_authorization.clone();
        let right_root_authorization = self.right_root_authorization.clone();
        let compare_method = self.compare_method;
        let sort_by = self.sort_by;
        let sort_order = self.sort_order;
        let cancel_flag = self.cancel_flag.clone();

        thread::spawn(move || {
            // Phase 1: Count total items (with live progress)
            let counting_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let total = count_entries_recursive(
                &left_root,
                &right_root,
                "",
                &cancel_flag,
                &progress_tx,
                &counting_counter,
            );
            if cancel_flag.load(Ordering::Relaxed) {
                return;
            }

            // Phase 2: Build the diff list with progress
            let mut entries = Vec::new();
            let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            build_recursive_threaded(
                &left_root,
                &right_root,
                left_root_authorization.as_ref(),
                right_root_authorization.as_ref(),
                "",
                0,
                compare_method,
                sort_by,
                sort_order,
                &mut entries,
                &cancel_flag,
                &progress_tx,
                total,
                &counter,
            );

            if !cancel_flag.load(Ordering::Relaxed) {
                let _ = result_tx.send(DiffCompareResult(entries));
            }
        });
    }

    /// Poll for comparison progress and results.
    /// Returns true when comparison just completed this tick.
    pub fn poll(&mut self) -> bool {
        if !self.is_comparing {
            return false;
        }

        // Drain progress messages
        if let Some(ref progress_rx) = self.progress_receiver {
            loop {
                match progress_rx.try_recv() {
                    Ok(DiffProgressMsg::Counting(total)) => {
                        self.progress_total = total;
                    }
                    Ok(DiffProgressMsg::Comparing(path, count, total)) => {
                        self.progress_current = path;
                        self.progress_count = count;
                        self.progress_total = total;
                    }
                    Err(_) => break,
                }
            }
        }

        // Check for completion
        if let Some(ref receiver) = self.receiver {
            match receiver.try_recv() {
                Ok(DiffCompareResult(entries)) => {
                    self.all_entries = entries;
                    // Collapse all directories by default
                    self.collapsed_dirs.clear();
                    for entry in &self.all_entries {
                        if entry.is_directory {
                            self.collapsed_dirs.insert(entry.relative_path.clone());
                        }
                    }
                    let current_paths: HashSet<_> = self
                        .all_entries
                        .iter()
                        .map(|entry| entry.relative_path.as_str())
                        .collect();
                    self.selected_files
                        .retain(|path| current_paths.contains(path.as_str()));
                    self.apply_filter();
                    self.is_comparing = false;
                    self.receiver = None;
                    self.progress_receiver = None;
                    return true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.is_comparing = false;
                    self.receiver = None;
                    self.progress_receiver = None;
                }
            }
        }
        false
    }

    /// Returns true if there are any differences (Modified, LeftOnly, RightOnly, DirModified)
    pub fn has_differences(&self) -> bool {
        self.all_entries.iter().any(|e| {
            matches!(
                e.status,
                DiffStatus::Modified
                    | DiffStatus::LeftOnly
                    | DiffStatus::RightOnly
                    | DiffStatus::DirModified
            )
        })
    }

    /// Cancel ongoing comparison
    pub fn cancel(&mut self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
        self.is_comparing = false;
        self.receiver = None;
        self.progress_receiver = None;
        self.copy_prompt = None;
        self.pending_copy_path = None;
        self.delete_prompt = None;
        self.pending_delete_path = None;
    }

    /// Build the flat diff list by recursively comparing both directory trees (synchronous)
    pub fn build_diff_list(&mut self) {
        self.all_entries.clear();
        let left_root = self.left_root.clone();
        let right_root = self.right_root.clone();
        self.left_root_authorization = file_ops::capture_directory_authorization(&left_root).ok();
        self.right_root_authorization = file_ops::capture_directory_authorization(&right_root).ok();
        let left_root_authorization = self.left_root_authorization.clone();
        let right_root_authorization = self.right_root_authorization.clone();
        build_recursive(
            &left_root,
            &right_root,
            left_root_authorization.as_ref(),
            right_root_authorization.as_ref(),
            "",
            0,
            self.compare_method,
            self.sort_by,
            self.sort_order,
            &mut self.all_entries,
        );
        let current_paths: HashSet<_> = self
            .all_entries
            .iter()
            .map(|entry| entry.relative_path.as_str())
            .collect();
        self.selected_files
            .retain(|path| current_paths.contains(path.as_str()));
        // Collapse all directories by default
        self.collapsed_dirs.clear();
        for entry in &self.all_entries {
            if entry.is_directory {
                self.collapsed_dirs.insert(entry.relative_path.clone());
            }
        }
    }

    /// Rebuild filtered_indices based on the current filter and collapsed state
    pub fn apply_filter(&mut self) {
        self.filtered_indices.clear();

        // First, determine which entries match the filter
        let mut matching_indices: HashSet<usize> = HashSet::new();

        for (i, entry) in self.all_entries.iter().enumerate() {
            let matches = match self.filter {
                DiffFilter::All => true,
                DiffFilter::DifferentOnly => matches!(
                    entry.status,
                    DiffStatus::Modified
                        | DiffStatus::LeftOnly
                        | DiffStatus::RightOnly
                        | DiffStatus::DirModified
                ),
                DiffFilter::LeftOnly => entry.status == DiffStatus::LeftOnly,
                DiffFilter::RightOnly => entry.status == DiffStatus::RightOnly,
            };

            if matches {
                matching_indices.insert(i);
            }
        }

        // Also include parent directories of matching items
        if self.filter != DiffFilter::All {
            let mut parent_indices: HashSet<usize> = HashSet::new();
            for &idx in &matching_indices {
                let entry = &self.all_entries[idx];
                if entry.depth > 0 {
                    // Walk backwards to find parent directories
                    let parts: Vec<&str> = entry.relative_path.rsplitn(2, '/').collect();
                    if parts.len() > 1 {
                        let parent_path = parts[1];
                        for (j, other) in self.all_entries.iter().enumerate() {
                            if other.is_directory && other.relative_path == parent_path {
                                parent_indices.insert(j);
                            }
                        }
                    }
                    // Also include all ancestor directories
                    let mut current_path = entry.relative_path.as_str();
                    while let Some(pos) = current_path.rfind('/') {
                        current_path = &current_path[..pos];
                        for (j, other) in self.all_entries.iter().enumerate() {
                            if other.is_directory && other.relative_path == current_path {
                                parent_indices.insert(j);
                            }
                        }
                    }
                }
            }
            matching_indices.extend(parent_indices);
        }

        // Build filtered_indices in order, skipping children of collapsed directories
        for i in 0..self.all_entries.len() {
            if !matching_indices.contains(&i) {
                continue;
            }

            let entry = &self.all_entries[i];

            // Check if any ancestor directory is collapsed
            let hidden = self.is_hidden_by_collapsed_ancestor(entry);
            if hidden {
                continue;
            }

            self.filtered_indices.push(i);
        }

        // Reset cursor if out of bounds
        if self.selected_index >= self.filtered_indices.len() {
            self.selected_index = self.filtered_indices.len().saturating_sub(1);
        }
        self.scroll_offset = 0;
    }

    /// Check if an entry is hidden because one of its ancestor directories is collapsed
    fn is_hidden_by_collapsed_ancestor(&self, entry: &DiffEntry) -> bool {
        if entry.depth == 0 {
            return false;
        }
        // Walk up the path to check each ancestor
        let mut current_path = entry.relative_path.as_str();
        while let Some(pos) = current_path.rfind('/') {
            let parent_path = &current_path[..pos];
            if self.collapsed_dirs.contains(parent_path) {
                return true;
            }
            current_path = parent_path;
        }
        false
    }

    /// Toggle collapse/expand state for a directory
    pub fn toggle_collapse(&mut self) {
        if let Some(entry) = self.current_entry() {
            if entry.is_directory {
                let path = entry.relative_path.clone();
                // Remember the all_entries index of the current entry to restore cursor position
                let current_all_idx = self.filtered_indices.get(self.selected_index).copied();
                if self.collapsed_dirs.contains(&path) {
                    // Expanding: lazy-load children if needed
                    if let Some(all_idx) = current_all_idx {
                        if self.all_entries[all_idx].children_not_loaded {
                            self.lazy_load_children(all_idx);
                        }
                    }
                    self.collapsed_dirs.remove(&path);
                } else {
                    // When collapsing, also collapse all descendant directories
                    let prefix = format!("{}/", path);
                    let descendants: Vec<String> = self
                        .all_entries
                        .iter()
                        .filter(|e| e.is_directory && e.relative_path.starts_with(&prefix))
                        .map(|e| e.relative_path.clone())
                        .collect();
                    for d in descendants {
                        self.collapsed_dirs.insert(d);
                    }
                    self.collapsed_dirs.insert(path);
                }
                self.apply_filter();
                // Restore cursor to the same entry
                if let Some(all_idx) = current_all_idx {
                    for (i, &fi) in self.filtered_indices.iter().enumerate() {
                        if fi == all_idx {
                            self.selected_index = i;
                            break;
                        }
                    }
                }
                // Adjust scroll to keep cursor visible (will be finalized in draw)
                if self.visible_height > 0 {
                    if self.selected_index < self.scroll_offset {
                        self.scroll_offset = self.selected_index;
                    } else if self.selected_index >= self.scroll_offset + self.visible_height {
                        self.scroll_offset =
                            self.selected_index.saturating_sub(self.visible_height - 1);
                    }
                }
            }
        }
    }

    /// Expand the current directory by one level (Right arrow)
    /// Only expands if collapsed; child directories remain collapsed
    pub fn expand_one_level(&mut self) {
        if let Some(entry) = self.current_entry() {
            if entry.is_directory && self.collapsed_dirs.contains(&entry.relative_path) {
                let path = entry.relative_path.clone();
                let current_all_idx = self.filtered_indices.get(self.selected_index).copied();
                // Lazy-load children if needed
                if let Some(all_idx) = current_all_idx {
                    if self.all_entries[all_idx].children_not_loaded {
                        self.lazy_load_children(all_idx);
                    }
                }
                self.collapsed_dirs.remove(&path);
                self.apply_filter();
                if let Some(all_idx) = current_all_idx {
                    for (i, &fi) in self.filtered_indices.iter().enumerate() {
                        if fi == all_idx {
                            self.selected_index = i;
                            break;
                        }
                    }
                }
                if self.visible_height > 0 {
                    if self.selected_index < self.scroll_offset {
                        self.scroll_offset = self.selected_index;
                    } else if self.selected_index >= self.scroll_offset + self.visible_height {
                        self.scroll_offset =
                            self.selected_index.saturating_sub(self.visible_height - 1);
                    }
                }
            }
        }
    }

    /// Close the current tree branch by one level (Left arrow).
    /// An expanded directory collapses in place. From a file or an already
    /// collapsed directory, its parent is focused and collapsed immediately.
    pub fn collapse_one_level(&mut self) {
        let Some(current_all_idx) = self.filtered_indices.get(self.selected_index).copied() else {
            return;
        };
        let Some(current) = self.all_entries.get(current_all_idx) else {
            return;
        };

        let target_path =
            if current.is_directory && !self.collapsed_dirs.contains(&current.relative_path) {
                current.relative_path.clone()
            } else {
                let Some((parent, _)) = current.relative_path.rsplit_once('/') else {
                    return;
                };
                parent.to_string()
            };
        let Some(target_all_idx) = self
            .all_entries
            .iter()
            .position(|entry| entry.is_directory && entry.relative_path == target_path)
        else {
            return;
        };

        let prefix = format!("{}/", target_path);
        let descendants: Vec<String> = self
            .all_entries
            .iter()
            .filter(|entry| entry.is_directory && entry.relative_path.starts_with(&prefix))
            .map(|entry| entry.relative_path.clone())
            .collect();
        self.collapsed_dirs.extend(descendants);
        self.collapsed_dirs.insert(target_path);
        self.apply_filter();

        if let Some(index) = self
            .filtered_indices
            .iter()
            .position(|&all_idx| all_idx == target_all_idx)
        {
            self.selected_index = index;
        }
        if self.visible_height > 0 {
            if self.selected_index < self.scroll_offset {
                self.scroll_offset = self.selected_index;
            } else if self.selected_index >= self.scroll_offset + self.visible_height {
                self.scroll_offset = self.selected_index.saturating_sub(self.visible_height - 1);
            }
        }
    }

    /// Expand all subdirectories under the current directory
    pub fn expand_all(&mut self) {
        if let Some(entry) = self.current_entry() {
            if entry.is_directory {
                let path = entry.relative_path.clone();
                let current_all_idx = self.filtered_indices.get(self.selected_index).copied();
                // Lazy-load this directory and all descendants recursively
                if let Some(all_idx) = current_all_idx {
                    self.lazy_load_all_descendants(all_idx);
                }
                // Remove this directory and all descendants from collapsed_dirs
                let prefix = format!("{}/", path);
                self.collapsed_dirs.remove(&path);
                let descendants: Vec<String> = self
                    .collapsed_dirs
                    .iter()
                    .filter(|p| p.starts_with(&prefix))
                    .cloned()
                    .collect();
                for d in descendants {
                    self.collapsed_dirs.remove(&d);
                }
                self.apply_filter();
                // Restore cursor
                if let Some(all_idx) = current_all_idx {
                    for (i, &fi) in self.filtered_indices.iter().enumerate() {
                        if fi == all_idx {
                            self.selected_index = i;
                            break;
                        }
                    }
                }
                if self.visible_height > 0 {
                    if self.selected_index < self.scroll_offset {
                        self.scroll_offset = self.selected_index;
                    } else if self.selected_index >= self.scroll_offset + self.visible_height {
                        self.scroll_offset =
                            self.selected_index.saturating_sub(self.visible_height - 1);
                    }
                }
            }
        }
    }

    /// Collapse the current directory (and all its descendants)
    pub fn collapse(&mut self) {
        if let Some(entry) = self.current_entry() {
            if entry.is_directory {
                let path = entry.relative_path.clone();
                let current_all_idx = self.filtered_indices.get(self.selected_index).copied();
                // Collapse this directory and all descendant directories
                let prefix = format!("{}/", path);
                let descendants: Vec<String> = self
                    .all_entries
                    .iter()
                    .filter(|e| e.is_directory && e.relative_path.starts_with(&prefix))
                    .map(|e| e.relative_path.clone())
                    .collect();
                for d in descendants {
                    self.collapsed_dirs.insert(d);
                }
                self.collapsed_dirs.insert(path);
                self.apply_filter();
                // Restore cursor
                if let Some(all_idx) = current_all_idx {
                    for (i, &fi) in self.filtered_indices.iter().enumerate() {
                        if fi == all_idx {
                            self.selected_index = i;
                            break;
                        }
                    }
                }
                if self.visible_height > 0 {
                    if self.selected_index < self.scroll_offset {
                        self.scroll_offset = self.selected_index;
                    } else if self.selected_index >= self.scroll_offset + self.visible_height {
                        self.scroll_offset =
                            self.selected_index.saturating_sub(self.visible_height - 1);
                    }
                }
            }
        }
    }

    /// Lazily load children for a one-side-only directory that hasn't been expanded yet.
    /// Inserts child entries right after the directory entry in all_entries.
    fn lazy_load_children(&mut self, all_entry_idx: usize) {
        let _ = self.lazy_load_children_with_policy(all_entry_idx, false);
    }

    fn lazy_load_children_checked(&mut self, all_entry_idx: usize) -> io::Result<()> {
        self.lazy_load_children_with_policy(all_entry_idx, true)
    }

    fn lazy_load_children_with_policy(
        &mut self,
        all_entry_idx: usize,
        strict_reads: bool,
    ) -> io::Result<()> {
        let entry = &self.all_entries[all_entry_idx];
        if !entry.children_not_loaded || !entry.is_directory {
            return Ok(());
        }

        let is_left = entry.status == DiffStatus::LeftOnly;
        let root = if is_left {
            self.left_root.clone()
        } else {
            self.right_root.clone()
        };
        let relative_path = entry.relative_path.clone();
        let parent_depth = entry.depth;

        // Load one level of children
        let dir_path = root.join(&relative_path);
        let names = if strict_reads {
            read_dir_names_checked(&dir_path)?
        } else {
            read_dir_names(&dir_path)
        };

        let mut sorted_names = names;
        sort_names_one_side(&mut sorted_names, &dir_path);

        let mut children = Vec::new();
        for name in &sorted_names {
            let child_relative = format!("{}/{}", relative_path, name);
            let full_path = dir_path.join(name);
            let info = make_file_info(&full_path, name);
            if strict_reads && info.is_none() {
                return Err(io::Error::other(format!(
                    "Expanded DIFF entry '{}' changed while it was refreshed",
                    child_relative
                )));
            }
            let is_dir = info.as_ref().map_or(false, |i| i.is_directory);
            let status = if is_left {
                DiffStatus::LeftOnly
            } else {
                DiffStatus::RightOnly
            };

            let (left, right) = if is_left { (info, None) } else { (None, info) };
            let left_missing = if left.is_none() {
                self.left_root_authorization.as_ref().and_then(|root| {
                    file_ops::capture_missing_path_authorization(
                        root,
                        Path::new(&child_relative),
                        "Diff left path",
                    )
                    .ok()
                })
            } else {
                None
            };
            let right_missing = if right.is_none() {
                self.right_root_authorization.as_ref().and_then(|root| {
                    file_ops::capture_missing_path_authorization(
                        root,
                        Path::new(&child_relative),
                        "Diff right path",
                    )
                    .ok()
                })
            } else {
                None
            };

            children.push(DiffEntry {
                relative_path: child_relative,
                left,
                right,
                status,
                is_directory: is_dir,
                depth: parent_depth + 1,
                children_not_loaded: is_dir,
                left_missing,
                right_missing,
            });
        }

        // Mark as loaded
        self.all_entries[all_entry_idx].children_not_loaded = false;

        // Insert children right after the parent entry
        let insert_pos = all_entry_idx + 1;
        // Update filtered_indices: shift indices >= insert_pos by children.len()
        let count = children.len();
        if count > 0 {
            for idx in self.filtered_indices.iter_mut() {
                if *idx >= insert_pos {
                    *idx += count;
                }
            }
            // Splice children into all_entries
            self.all_entries.splice(insert_pos..insert_pos, children);
            // Collapse newly loaded child directories by default
            for i in insert_pos..insert_pos + count {
                if self.all_entries[i].is_directory {
                    self.collapsed_dirs
                        .insert(self.all_entries[i].relative_path.clone());
                }
            }
        }
        Ok(())
    }

    /// Recursively lazy-load all descendants of a directory
    fn lazy_load_all_descendants(&mut self, all_entry_idx: usize) {
        if all_entry_idx >= self.all_entries.len() {
            return;
        }

        // Process higher-index siblings first. Inserting descendants after a
        // higher index cannot invalidate the lower sibling indices still on the
        // stack, avoiding both recursive stack growth and index-shift bugs.
        let mut pending = vec![all_entry_idx];
        while let Some(index) = pending.pop() {
            if index >= self.all_entries.len() {
                continue;
            }
            if self.all_entries[index].children_not_loaded {
                self.lazy_load_children(index);
            }

            let parent_path = self.all_entries[index].relative_path.clone();
            let parent_depth = self.all_entries[index].depth;
            let prefix = format!("{}/", parent_path);
            let mut child_directories = Vec::new();
            let mut i = index + 1;
            while i < self.all_entries.len() {
                if !self.all_entries[i].relative_path.starts_with(&prefix) {
                    break;
                }
                if self.all_entries[i].is_directory && self.all_entries[i].depth == parent_depth + 1
                {
                    child_directories.push(i);
                }
                i += 1;
            }
            // Ascending push means the highest index is popped first.
            pending.extend(child_directories);
        }
    }

    /// Move cursor by delta within filtered_indices bounds
    pub fn move_cursor(&mut self, delta: i32) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let new_index = (self.selected_index as i32 + delta)
            .max(0)
            .min(self.filtered_indices.len().saturating_sub(1) as i32)
            as usize;
        self.selected_index = new_index;
    }

    /// Move cursor to the first item
    pub fn cursor_to_start(&mut self) {
        self.selected_index = 0;
    }

    /// Move cursor to the last item
    pub fn cursor_to_end(&mut self) {
        if !self.filtered_indices.is_empty() {
            self.selected_index = self.filtered_indices.len() - 1;
        }
    }

    /// Adjust scroll offset so the selected item is visible
    pub fn adjust_scroll(&mut self, visible_height: usize) {
        self.visible_height = visible_height;
        if visible_height == 0 {
            return;
        }

        if self.selected_index < self.scroll_offset {
            self.scroll_offset = self.selected_index;
        } else if self.selected_index >= self.scroll_offset + visible_height {
            self.scroll_offset = self.selected_index - visible_height + 1;
        }
    }

    /// Toggle selection of the current item
    pub fn toggle_selection(&mut self) {
        if let Some(entry) = self.current_entry() {
            let key = entry.relative_path.clone();
            if self.selected_files.contains(&key) {
                self.selected_files.remove(&key);
            } else {
                self.selected_files.insert(key);
            }
        }
    }

    /// Get the entry at the current filtered index
    pub fn current_entry(&self) -> Option<&DiffEntry> {
        self.filtered_indices
            .get(self.selected_index)
            .and_then(|&idx| self.all_entries.get(idx))
    }

    pub(crate) fn copy_prompt_availability(&self) -> (bool, bool) {
        self.copy_prompt
            .as_ref()
            .map(|prompt| (prompt.left_item.is_some(), prompt.right_item.is_some()))
            .unwrap_or((false, false))
    }

    pub(crate) fn copy_in_progress(&self) -> bool {
        self.copy_in_progress
    }

    pub(crate) fn finish_copy_operation(&mut self) -> io::Result<()> {
        self.copy_in_progress = false;
        let Some(relative_path) = self.pending_copy_path.take() else {
            self.left_root_authorization = None;
            self.right_root_authorization = None;
            return Err(io::Error::other("Copy completion path is unavailable"));
        };
        let result = self.reconcile_operation_path(&relative_path, true);
        if result.is_err() {
            self.left_root_authorization = None;
            self.right_root_authorization = None;
        }
        result
    }

    pub(crate) fn delete_prompt_availability(&self) -> (bool, bool, bool) {
        self.delete_prompt
            .as_ref()
            .map(|prompt| {
                (
                    prompt.left_item.is_some(),
                    prompt.right_item.is_some(),
                    prompt.contains_directory,
                )
            })
            .unwrap_or((false, false, false))
    }

    pub(crate) fn delete_in_progress(&self) -> bool {
        self.delete_in_progress
    }

    pub(crate) fn finish_delete_operation(&mut self) -> io::Result<()> {
        self.delete_in_progress = false;
        let Some(relative_path) = self.pending_delete_path.take() else {
            self.left_root_authorization = None;
            self.right_root_authorization = None;
            return Err(io::Error::other("Delete completion path is unavailable"));
        };
        let result = self.reconcile_operation_path(&relative_path, false);
        if result.is_err() {
            self.left_root_authorization = None;
            self.right_root_authorization = None;
        }
        result
    }

    /// Re-read only the item touched by a DIFF mutation and its ancestor
    /// metadata. The rest of the comparison remains the snapshot the user was
    /// working with until they explicitly request another full comparison.
    fn reconcile_operation_path(
        &mut self,
        relative_path: &str,
        allow_ancestor_creation: bool,
    ) -> io::Result<()> {
        let relative = validated_diff_relative_path(relative_path)?;
        let target_index = self
            .all_entries
            .iter()
            .position(|entry| entry.relative_path == relative_path)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "Completed operation target '{relative_path}' is no longer in the comparison"
                    ),
                )
            })?;
        let target_depth = self.all_entries[target_index].depth;
        let target_end = diff_subtree_end(&self.all_entries, target_index);
        let cursor_anchor = self.capture_cursor_anchor();
        let expanded_dirs: HashSet<String> = self.all_entries[target_index..target_end]
            .iter()
            .filter(|entry| {
                entry.is_directory && !self.collapsed_dirs.contains(&entry.relative_path)
            })
            .map(|entry| entry.relative_path.clone())
            .collect();

        let roots = self.capture_refreshed_roots();
        let (left_root_authorization, right_root_authorization) = match roots {
            Ok(roots) => roots,
            Err(error) => {
                // A replaced comparison boundary invalidates every relative
                // authorization. Disable further mutations until a full
                // comparison deliberately establishes new boundaries.
                self.left_root_authorization = None;
                self.right_root_authorization = None;
                return Err(error);
            }
        };

        let replacement = build_targeted_subtree(
            &self.left_root,
            &self.right_root,
            &left_root_authorization,
            &right_root_authorization,
            relative_path,
            &relative,
            target_depth,
            self.compare_method,
            self.sort_by,
            self.sort_order,
        )?;

        // Stage the tree rewrite so any validation/read failure leaves the
        // displayed comparison untouched.
        let mut entries = self.all_entries.clone();
        entries.splice(target_index..target_end, replacement);
        let created_ancestors = refresh_diff_ancestor_entries(
            &mut entries,
            &self.left_root,
            &self.right_root,
            &left_root_authorization,
            &right_root_authorization,
            relative_path,
            self.compare_method,
            allow_ancestor_creation,
        )?;
        refresh_created_ancestor_missing_authorizations(
            &mut entries,
            &left_root_authorization,
            &right_root_authorization,
            &created_ancestors,
        );

        let mut collapsed_dirs = self.collapsed_dirs.clone();
        collapsed_dirs.retain(|path| !diff_path_is_within(path, relative_path));
        for entry in &entries {
            if entry.is_directory && diff_path_is_within(&entry.relative_path, relative_path) {
                collapsed_dirs.insert(entry.relative_path.clone());
            }
        }

        // Restoring an expanded one-sided directory may lazily read its
        // children. Install the staged state only for that synchronous work,
        // then validate the roots once more before allowing the new state to
        // become visible. A failed boundary check restores the prior snapshot.
        let previous_entries = std::mem::replace(&mut self.all_entries, entries);
        let previous_collapsed_dirs = std::mem::replace(&mut self.collapsed_dirs, collapsed_dirs);
        let previous_filtered_indices = std::mem::take(&mut self.filtered_indices);
        self.left_root_authorization = Some(left_root_authorization);
        self.right_root_authorization = Some(right_root_authorization);
        if let Err(error) = self.restore_expanded_directories(expanded_dirs) {
            self.all_entries = previous_entries;
            self.collapsed_dirs = previous_collapsed_dirs;
            self.filtered_indices = previous_filtered_indices;
            return Err(error);
        }
        self.all_entries = resort_flat_tree(&self.all_entries, self.sort_by, self.sort_order);

        let left_root = self.left_root.clone();
        let right_root = self.right_root.clone();
        let mut selected_files = self.selected_files.clone();
        selected_files.retain(|path| {
            !diff_path_is_within(path, relative_path)
                || !path_is_definitely_missing(&left_root.join(path))
                || !path_is_definitely_missing(&right_root.join(path))
        });

        // Rebind the roots after every targeted read. If either boundary was
        // swapped while the subtree, restored expansions, or retained
        // selections were being inspected, do not publish a mixed snapshot.
        let (left_root_authorization, right_root_authorization) =
            match self.capture_refreshed_roots() {
                Ok(roots) => roots,
                Err(error) => {
                    self.all_entries = previous_entries;
                    self.collapsed_dirs = previous_collapsed_dirs;
                    self.filtered_indices = previous_filtered_indices;
                    self.left_root_authorization = None;
                    self.right_root_authorization = None;
                    return Err(error);
                }
            };

        self.left_root_authorization = Some(left_root_authorization);
        self.right_root_authorization = Some(right_root_authorization);
        self.selected_files = selected_files;
        self.apply_filter();
        self.restore_cursor_anchor(cursor_anchor);
        Ok(())
    }

    fn capture_refreshed_roots(
        &self,
    ) -> io::Result<(
        file_ops::DirectoryAuthorization,
        file_ops::DirectoryAuthorization,
    )> {
        let expected_left = self.left_root_authorization.as_ref().ok_or_else(|| {
            io::Error::other("Diff left root identity is unavailable; restart the comparison")
        })?;
        let expected_right = self.right_root_authorization.as_ref().ok_or_else(|| {
            io::Error::other("Diff right root identity is unavailable; restart the comparison")
        })?;
        let current_left =
            file_ops::capture_directory_authorization(&self.left_root).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("Cannot refresh the DIFF left root safely: {error}"),
                )
            })?;
        let current_right =
            file_ops::capture_directory_authorization(&self.right_root).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("Cannot refresh the DIFF right root safely: {error}"),
                )
            })?;
        if !expected_left.same_object(&current_left) || !expected_right.same_object(&current_right)
        {
            return Err(io::Error::other(
                "A comparison root was replaced; run the comparison again",
            ));
        }
        ensure_non_overlapping_diff_roots(&current_left, &current_right)?;
        Ok((current_left, current_right))
    }

    fn capture_cursor_anchor(&self) -> DiffCursorAnchor {
        let visible_paths: Vec<String> = self
            .filtered_indices
            .iter()
            .filter_map(|&index| self.all_entries.get(index))
            .map(|entry| entry.relative_path.clone())
            .collect();
        let focused_path = visible_paths.get(self.selected_index).cloned();
        let following_paths = visible_paths
            .iter()
            .skip(self.selected_index.saturating_add(1))
            .cloned()
            .collect();
        let preceding_paths = visible_paths
            .iter()
            .take(self.selected_index)
            .rev()
            .cloned()
            .collect();
        let parent_path = focused_path
            .as_deref()
            .and_then(|path| path.rsplit_once('/').map(|(parent, _)| parent.to_string()));

        DiffCursorAnchor {
            focused_path,
            following_paths,
            preceding_paths,
            parent_path,
            visual_row: self.selected_index.saturating_sub(self.scroll_offset),
            previous_index: self.selected_index,
            previous_scroll: self.scroll_offset,
        }
    }

    fn restore_cursor_anchor(&mut self, anchor: DiffCursorAnchor) {
        if self.filtered_indices.is_empty() {
            self.selected_index = 0;
            self.scroll_offset = 0;
            return;
        }

        let positions: HashMap<&str, usize> = self
            .filtered_indices
            .iter()
            .enumerate()
            .filter_map(|(visible_index, &entry_index)| {
                self.all_entries
                    .get(entry_index)
                    .map(|entry| (entry.relative_path.as_str(), visible_index))
            })
            .collect();
        let position_of = |path: &str| positions.get(path).copied();
        let selected = anchor
            .focused_path
            .as_deref()
            .and_then(position_of)
            .or_else(|| {
                anchor
                    .following_paths
                    .iter()
                    .find_map(|path| position_of(path))
            })
            .or_else(|| {
                anchor
                    .preceding_paths
                    .iter()
                    .find_map(|path| position_of(path))
            })
            .or_else(|| anchor.parent_path.as_deref().and_then(position_of))
            .unwrap_or_else(|| anchor.previous_index.min(self.filtered_indices.len() - 1));
        self.selected_index = selected;

        let viewport_height = self.visible_height.max(1);
        let max_scroll = self.filtered_indices.len().saturating_sub(viewport_height);
        let visual_row = anchor.visual_row.min(viewport_height - 1);
        let mut scroll = if anchor.focused_path.is_some() {
            selected.saturating_sub(visual_row)
        } else {
            anchor.previous_scroll
        }
        .min(max_scroll);
        if selected < scroll {
            scroll = selected;
        } else if selected >= scroll.saturating_add(viewport_height) {
            scroll = selected.saturating_sub(viewport_height - 1);
        }
        self.scroll_offset = scroll;
    }

    fn restore_expanded_directories(&mut self, expanded_dirs: HashSet<String>) -> io::Result<()> {
        let mut expanded_dirs: Vec<_> = expanded_dirs.into_iter().collect();
        expanded_dirs.sort_by(|left, right| {
            left.matches('/')
                .count()
                .cmp(&right.matches('/').count())
                .then_with(|| left.cmp(right))
        });

        for path in expanded_dirs {
            let Some(index) = self
                .all_entries
                .iter()
                .position(|entry| entry.is_directory && entry.relative_path == path)
            else {
                continue;
            };
            if self.all_entries[index].children_not_loaded {
                self.lazy_load_children_checked(index)?;
            }
            self.collapsed_dirs.remove(&path);
        }
        Ok(())
    }

    /// Re-sort all_entries in memory (preserving DFS tree structure) and reapply filter
    pub fn resort_entries(&mut self) {
        if self.all_entries.is_empty() {
            return;
        }
        let sorted = resort_flat_tree(&self.all_entries, self.sort_by, self.sort_order);
        self.all_entries = sorted;
        self.apply_filter();
    }
}

fn diff_path_is_within(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|remainder| remainder.starts_with('/'))
}

fn diff_subtree_end(entries: &[DiffEntry], root_index: usize) -> usize {
    let root_depth = entries[root_index].depth;
    entries
        .iter()
        .enumerate()
        .skip(root_index + 1)
        .find_map(|(index, entry)| (entry.depth <= root_depth).then_some(index))
        .unwrap_or(entries.len())
}

fn path_is_definitely_missing(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(path),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            )
    )
}

fn capture_diff_path_snapshot(
    filesystem_root: &Path,
    root_authorization: &file_ops::DirectoryAuthorization,
    relative_path: &Path,
    relative_key: &str,
    role: &str,
) -> io::Result<(
    Option<DiffFileInfo>,
    Option<file_ops::MissingPathAuthorization>,
)> {
    let name = relative_path
        .file_name()
        .ok_or_else(|| io::Error::other("DIFF refresh path has no file name"))?
        .to_string_lossy();
    let info = make_file_info(&filesystem_root.join(relative_path), &name);
    let missing = if info.is_none() {
        Some(file_ops::capture_missing_path_authorization(
            root_authorization,
            Path::new(relative_key),
            role,
        )?)
    } else {
        None
    };
    Ok((info, missing))
}

#[allow(clippy::too_many_arguments)]
fn build_targeted_subtree(
    left_root: &Path,
    right_root: &Path,
    left_root_authorization: &file_ops::DirectoryAuthorization,
    right_root_authorization: &file_ops::DirectoryAuthorization,
    relative_key: &str,
    relative_path: &Path,
    depth: usize,
    compare_method: CompareMethod,
    sort_by: SortBy,
    sort_order: SortOrder,
) -> io::Result<Vec<DiffEntry>> {
    let (left, left_missing) = capture_diff_path_snapshot(
        left_root,
        left_root_authorization,
        relative_path,
        relative_key,
        "Diff left refresh path",
    )?;
    let (right, right_missing) = capture_diff_path_snapshot(
        right_root,
        right_root_authorization,
        relative_path,
        relative_key,
        "Diff right refresh path",
    )?;

    if left.is_none() && right.is_none() {
        return Ok(Vec::new());
    }

    let left_is_dir = left.as_ref().is_some_and(|info| info.is_directory);
    let right_is_dir = right.as_ref().is_some_and(|info| info.is_directory);
    let is_directory = left_is_dir || right_is_dir;
    let status = match (&left, &right) {
        (Some(_), None) => DiffStatus::LeftOnly,
        (None, Some(_)) => DiffStatus::RightOnly,
        (Some(_), Some(_)) if left_is_dir && right_is_dir => DiffStatus::DirSame,
        (Some(left), Some(right)) if !left_is_dir && !right_is_dir => {
            if compare_files(left, right, compare_method) {
                DiffStatus::Same
            } else {
                DiffStatus::Modified
            }
        }
        (Some(_), Some(_)) => DiffStatus::Modified,
        (None, None) => unreachable!("both-absent target returned above"),
    };
    let both_directories = left_is_dir && right_is_dir;
    let is_one_sided_directory =
        is_directory && matches!(status, DiffStatus::LeftOnly | DiffStatus::RightOnly);
    let mut entries = vec![DiffEntry {
        relative_path: relative_key.to_string(),
        left,
        right,
        status,
        is_directory,
        depth,
        children_not_loaded: is_one_sided_directory,
        left_missing,
        right_missing,
    }];

    if both_directories {
        build_iterative_from(
            left_root,
            right_root,
            Some(left_root_authorization),
            Some(right_root_authorization),
            relative_key.to_string(),
            depth + 1,
            Some(0),
            compare_method,
            sort_by,
            sort_order,
            &mut entries,
            None,
            true,
        )?;
    }
    Ok(entries)
}

fn diff_info_is_same_object(old: &DiffFileInfo, current: &DiffFileInfo) -> bool {
    match (old.authorization.as_ref(), current.authorization.as_ref()) {
        (Some(old), Some(current)) => old.same_object(current),
        _ => false,
    }
}

fn diff_status_has_difference(status: DiffStatus) -> bool {
    !matches!(status, DiffStatus::Same | DiffStatus::DirSame)
}

#[allow(clippy::too_many_arguments)]
fn refresh_diff_ancestor_entries(
    entries: &mut [DiffEntry],
    left_root: &Path,
    right_root: &Path,
    left_root_authorization: &file_ops::DirectoryAuthorization,
    right_root_authorization: &file_ops::DirectoryAuthorization,
    relative_key: &str,
    compare_method: CompareMethod,
    allow_ancestor_creation: bool,
) -> io::Result<CreatedAncestorSides> {
    let mut child_path = relative_key;
    let mut ancestors = Vec::new();
    let mut created = CreatedAncestorSides::default();
    while let Some((parent, _)) = child_path.rsplit_once('/') {
        ancestors.push(parent.to_string());
        child_path = parent;
    }

    // The closest ancestor is refreshed first so each parent can derive its
    // status from already-updated descendants.
    for ancestor_key in ancestors {
        let index = entries
            .iter()
            .position(|entry| entry.relative_path == ancestor_key)
            .ok_or_else(|| {
                io::Error::other(format!(
                    "DIFF ancestor '{ancestor_key}' changed; run the comparison again"
                ))
            })?;
        let old_left = entries[index].left.as_ref();
        let old_right = entries[index].right.as_ref();
        let left_was_missing = old_left.is_none();
        let right_was_missing = old_right.is_none();
        let old_children_not_loaded = entries[index].children_not_loaded;
        let ancestor_path = Path::new(&ancestor_key);
        let (left, left_missing) = capture_diff_path_snapshot(
            left_root,
            left_root_authorization,
            ancestor_path,
            &ancestor_key,
            "Diff left ancestor refresh",
        )?;
        let (right, right_missing) = capture_diff_path_snapshot(
            right_root,
            right_root_authorization,
            ancestor_path,
            &ancestor_key,
            "Diff right ancestor refresh",
        )?;

        for (side, old, current) in [
            ("left", old_left, left.as_ref()),
            ("right", old_right, right.as_ref()),
        ] {
            match (old, current) {
                (Some(old), Some(current)) if !diff_info_is_same_object(old, current) => {
                    return Err(io::Error::other(format!(
                        "DIFF {side} ancestor '{ancestor_key}' was replaced; run the comparison again"
                    )));
                }
                (Some(_), None) => {
                    return Err(io::Error::other(format!(
                        "DIFF {side} ancestor '{ancestor_key}' disappeared; run the comparison again"
                    )));
                }
                (None, Some(_)) if !allow_ancestor_creation => {
                    return Err(io::Error::other(format!(
                        "DIFF {side} ancestor '{ancestor_key}' appeared unexpectedly; run the comparison again"
                    )));
                }
                _ => {}
            }
        }
        if left.is_none() && right.is_none() {
            return Err(io::Error::other(format!(
                "DIFF ancestor '{ancestor_key}' disappeared; run the comparison again"
            )));
        }
        if left_was_missing && left.is_some() {
            created.left.push(ancestor_key.clone());
        }
        if right_was_missing && right.is_some() {
            created.right.push(ancestor_key.clone());
        }

        let left_is_dir = left.as_ref().is_some_and(|info| info.is_directory);
        let right_is_dir = right.as_ref().is_some_and(|info| info.is_directory);
        let is_directory = left_is_dir || right_is_dir;
        let end = diff_subtree_end(entries, index);
        let descendants_differ = entries[index + 1..end]
            .iter()
            .any(|entry| diff_status_has_difference(entry.status));
        let status = match (&left, &right) {
            (Some(_), None) => DiffStatus::LeftOnly,
            (None, Some(_)) => DiffStatus::RightOnly,
            (Some(_), Some(_)) if left_is_dir && right_is_dir => {
                if descendants_differ {
                    DiffStatus::DirModified
                } else {
                    DiffStatus::DirSame
                }
            }
            (Some(left), Some(right)) if !left_is_dir && !right_is_dir => {
                if compare_files(left, right, compare_method) {
                    DiffStatus::Same
                } else {
                    DiffStatus::Modified
                }
            }
            (Some(_), Some(_)) => DiffStatus::Modified,
            (None, None) => unreachable!("both-absent ancestor rejected above"),
        };
        let entry = &mut entries[index];
        entry.left = left;
        entry.right = right;
        entry.status = status;
        entry.is_directory = is_directory;
        entry.children_not_loaded = is_directory
            && matches!(status, DiffStatus::LeftOnly | DiffStatus::RightOnly)
            && old_children_not_loaded;
        entry.left_missing = left_missing;
        entry.right_missing = right_missing;
    }
    Ok(created)
}

fn refresh_created_ancestor_missing_authorizations(
    entries: &mut [DiffEntry],
    left_root_authorization: &file_ops::DirectoryAuthorization,
    right_root_authorization: &file_ops::DirectoryAuthorization,
    created: &CreatedAncestorSides,
) {
    for entry in entries {
        if entry.left.is_none()
            && created
                .left
                .iter()
                .any(|ancestor| diff_path_is_within(&entry.relative_path, ancestor))
        {
            // A sibling may have appeared independently. In that case keep
            // the displayed snapshot but remove its mutation authorization.
            entry.left_missing = file_ops::capture_missing_path_authorization(
                left_root_authorization,
                Path::new(&entry.relative_path),
                "Diff left path after parent creation",
            )
            .ok();
        }
        if entry.right.is_none()
            && created
                .right
                .iter()
                .any(|ancestor| diff_path_is_within(&entry.relative_path, ancestor))
        {
            entry.right_missing = file_ops::capture_missing_path_authorization(
                right_root_authorization,
                Path::new(&entry.relative_path),
                "Diff right path after parent creation",
            )
            .ok();
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Recursive diff tree builder
// ═══════════════════════════════════════════════════════════════════════════════

struct BuildFrame {
    relative_path: String,
    left_dir: PathBuf,
    right_dir: PathBuf,
    names: Vec<String>,
    left_names: HashSet<String>,
    right_names: HashSet<String>,
    next_name: usize,
    depth: usize,
    owner_index: Option<usize>,
    has_difference: bool,
}

struct BuildProgress<'a> {
    cancel_flag: &'a AtomicBool,
    progress_tx: &'a Sender<DiffProgressMsg>,
    total: usize,
    counter: &'a std::sync::atomic::AtomicUsize,
}

fn make_build_frame(
    left_root: &Path,
    right_root: &Path,
    relative_path: String,
    depth: usize,
    owner_index: Option<usize>,
    sort_by: SortBy,
    sort_order: SortOrder,
    strict_reads: bool,
) -> io::Result<BuildFrame> {
    let left_dir = if relative_path.is_empty() {
        left_root.to_path_buf()
    } else {
        left_root.join(&relative_path)
    };
    let right_dir = if relative_path.is_empty() {
        right_root.to_path_buf()
    } else {
        right_root.join(&relative_path)
    };
    let read_names = |dir: &Path| {
        if strict_reads {
            read_dir_names_checked(dir).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("Cannot refresh DIFF directory '{}': {error}", dir.display()),
                )
            })
        } else {
            Ok(read_dir_names(dir))
        }
    };
    let left_names_vec = read_names(&left_dir)?;
    let right_names_vec = read_names(&right_dir)?;
    let left_refs: HashSet<&str> = left_names_vec.iter().map(String::as_str).collect();
    let right_refs: HashSet<&str> = right_names_vec.iter().map(String::as_str).collect();
    let mut names: Vec<String> = left_names_vec
        .iter()
        .chain(&right_names_vec)
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    sort_names(
        &mut names,
        &left_dir,
        &right_dir,
        &left_refs,
        &right_refs,
        sort_by,
        sort_order,
    );

    Ok(BuildFrame {
        relative_path,
        left_dir,
        right_dir,
        names,
        left_names: left_names_vec.into_iter().collect(),
        right_names: right_names_vec.into_iter().collect(),
        next_name: 0,
        depth,
        owner_index,
        has_difference: false,
    })
}

fn build_iterative(
    left_root: &Path,
    right_root: &Path,
    left_root_authorization: Option<&file_ops::DirectoryAuthorization>,
    right_root_authorization: Option<&file_ops::DirectoryAuthorization>,
    compare_method: CompareMethod,
    sort_by: SortBy,
    sort_order: SortOrder,
    entries: &mut Vec<DiffEntry>,
    progress: Option<BuildProgress<'_>>,
) {
    let _ = build_iterative_from(
        left_root,
        right_root,
        left_root_authorization,
        right_root_authorization,
        String::new(),
        0,
        None,
        compare_method,
        sort_by,
        sort_order,
        entries,
        progress,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
fn build_iterative_from(
    left_root: &Path,
    right_root: &Path,
    left_root_authorization: Option<&file_ops::DirectoryAuthorization>,
    right_root_authorization: Option<&file_ops::DirectoryAuthorization>,
    initial_relative_path: String,
    initial_depth: usize,
    initial_owner_index: Option<usize>,
    compare_method: CompareMethod,
    sort_by: SortBy,
    sort_order: SortOrder,
    entries: &mut Vec<DiffEntry>,
    progress: Option<BuildProgress<'_>>,
    strict_reads: bool,
) -> io::Result<()> {
    let mut frames = vec![make_build_frame(
        left_root,
        right_root,
        initial_relative_path,
        initial_depth,
        initial_owner_index,
        sort_by,
        sort_order,
        strict_reads,
    )?];

    while !frames.is_empty() {
        if progress
            .as_ref()
            .is_some_and(|p| p.cancel_flag.load(Ordering::Relaxed))
        {
            return Ok(());
        }

        let finished = frames
            .last()
            .map(|frame| frame.next_name >= frame.names.len())
            .unwrap_or(true);
        if finished {
            let finished_frame = frames.pop().expect("non-empty frame stack");
            let owner_index = finished_frame.owner_index;
            if let Some(dir_index) = owner_index {
                entries[dir_index].status = if finished_frame.has_difference {
                    DiffStatus::DirModified
                } else {
                    DiffStatus::DirSame
                };
                if finished_frame.has_difference {
                    if let Some(parent) = frames.last_mut() {
                        parent.has_difference = true;
                    }
                }
            }
            continue;
        }

        let (name, relative_path, left_dir, right_dir, depth, left_exists, right_exists) = {
            let frame = frames.last_mut().expect("non-empty frame stack");
            let name = frame.names[frame.next_name].clone();
            frame.next_name += 1;
            let relative_path = if frame.relative_path.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", frame.relative_path, name)
            };
            (
                name.clone(),
                relative_path,
                frame.left_dir.clone(),
                frame.right_dir.clone(),
                frame.depth,
                frame.left_names.contains(&name),
                frame.right_names.contains(&name),
            )
        };

        if let Some(progress) = progress.as_ref() {
            let count = progress.counter.fetch_add(1, Ordering::Relaxed) + 1;
            let _ = progress.progress_tx.send(DiffProgressMsg::Comparing(
                relative_path.clone(),
                count,
                progress.total,
            ));
        }

        let left_info = make_file_info(&left_dir.join(&name), &name);
        let right_info = make_file_info(&right_dir.join(&name), &name);
        if strict_reads && left_exists && left_info.is_none() {
            return Err(io::Error::other(format!(
                "DIFF left entry '{relative_path}' changed while it was refreshed"
            )));
        }
        if strict_reads && right_exists && right_info.is_none() {
            return Err(io::Error::other(format!(
                "DIFF right entry '{relative_path}' changed while it was refreshed"
            )));
        }
        let left_missing = if left_info.is_none() {
            left_root_authorization.and_then(|root| {
                file_ops::capture_missing_path_authorization(
                    root,
                    Path::new(&relative_path),
                    "Diff left path",
                )
                .ok()
            })
        } else {
            None
        };
        let right_missing = if right_info.is_none() {
            right_root_authorization.and_then(|root| {
                file_ops::capture_missing_path_authorization(
                    root,
                    Path::new(&relative_path),
                    "Diff right path",
                )
                .ok()
            })
        } else {
            None
        };
        let left_is_dir = left_info.as_ref().is_some_and(|info| info.is_directory);
        let right_is_dir = right_info.as_ref().is_some_and(|info| info.is_directory);
        let is_directory = left_is_dir || right_is_dir;

        if left_exists && right_exists {
            if left_is_dir && right_is_dir {
                let dir_index = entries.len();
                entries.push(DiffEntry {
                    relative_path: relative_path.clone(),
                    left: left_info,
                    right: right_info,
                    status: DiffStatus::DirSame,
                    is_directory: true,
                    depth,
                    children_not_loaded: false,
                    left_missing,
                    right_missing,
                });
                frames.push(make_build_frame(
                    left_root,
                    right_root,
                    relative_path,
                    depth + 1,
                    Some(dir_index),
                    sort_by,
                    sort_order,
                    strict_reads,
                )?);
            } else if !left_is_dir && !right_is_dir {
                let same = match (left_info.as_ref(), right_info.as_ref()) {
                    (Some(left), Some(right)) => compare_files(left, right, compare_method),
                    _ => false,
                };
                entries.push(DiffEntry {
                    relative_path,
                    left: left_info,
                    right: right_info,
                    status: if same {
                        DiffStatus::Same
                    } else {
                        DiffStatus::Modified
                    },
                    is_directory: false,
                    depth,
                    children_not_loaded: false,
                    left_missing,
                    right_missing,
                });
                if !same {
                    frames
                        .last_mut()
                        .expect("current directory frame")
                        .has_difference = true;
                }
            } else {
                entries.push(DiffEntry {
                    relative_path,
                    left: left_info,
                    right: right_info,
                    status: DiffStatus::Modified,
                    is_directory,
                    depth,
                    children_not_loaded: false,
                    left_missing,
                    right_missing,
                });
                frames
                    .last_mut()
                    .expect("current directory frame")
                    .has_difference = true;
            }
        } else {
            let status = if left_exists {
                DiffStatus::LeftOnly
            } else {
                DiffStatus::RightOnly
            };
            entries.push(DiffEntry {
                relative_path,
                left: if left_exists { left_info } else { None },
                right: if right_exists { right_info } else { None },
                status,
                is_directory,
                depth,
                children_not_loaded: is_directory,
                left_missing,
                right_missing,
            });
            frames
                .last_mut()
                .expect("current directory frame")
                .has_difference = true;
        }
    }
    Ok(())
}

fn build_recursive(
    left_root: &Path,
    right_root: &Path,
    left_root_authorization: Option<&file_ops::DirectoryAuthorization>,
    right_root_authorization: Option<&file_ops::DirectoryAuthorization>,
    relative_path: &str,
    depth: usize,
    compare_method: CompareMethod,
    sort_by: SortBy,
    sort_order: SortOrder,
    entries: &mut Vec<DiffEntry>,
) {
    // Kept as the synchronous entry point, but implemented with an explicit
    // frame stack so deeply nested directory trees cannot overflow the thread
    // stack. Callers currently invoke this only for the root.
    let left = if relative_path.is_empty() {
        left_root.to_path_buf()
    } else {
        left_root.join(relative_path)
    };
    let right = if relative_path.is_empty() {
        right_root.to_path_buf()
    } else {
        right_root.join(relative_path)
    };
    build_iterative(
        &left,
        &right,
        left_root_authorization,
        right_root_authorization,
        compare_method,
        sort_by,
        sort_order,
        entries,
        None,
    );

    if depth != 0 {
        for entry in entries.iter_mut() {
            entry.depth = entry.depth.saturating_add(depth);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Threaded diff builders (async with cancel + progress)
// ═══════════════════════════════════════════════════════════════════════════════

/// Check if path is a directory using the same logic as make_file_info
/// (symlink_metadata → metadata fallback), matching build_recursive_threaded behavior
fn is_dir_via_info(path: &Path) -> bool {
    make_file_info(path, "").map_or(false, |i| i.is_directory)
}

/// Count total entries in both directory trees (for progress bar total)
fn count_entries_recursive(
    left_root: &Path,
    right_root: &Path,
    relative_path: &str,
    cancel_flag: &AtomicBool,
    progress_tx: &Sender<DiffProgressMsg>,
    running_count: &Arc<std::sync::atomic::AtomicUsize>,
) -> usize {
    let mut pending = vec![relative_path.to_string()];

    while let Some(relative) = pending.pop() {
        if cancel_flag.load(Ordering::Relaxed) {
            break;
        }
        let left_dir = if relative.is_empty() {
            left_root.to_path_buf()
        } else {
            left_root.join(&relative)
        };
        let right_dir = if relative.is_empty() {
            right_root.to_path_buf()
        } else {
            right_root.join(&relative)
        };
        let left_names = read_dir_names(&left_dir);
        let right_names = read_dir_names(&right_dir);
        let left_set: HashSet<&str> = left_names.iter().map(String::as_str).collect();
        let right_set: HashSet<&str> = right_names.iter().map(String::as_str).collect();
        let all_names: HashSet<String> = left_names.iter().chain(&right_names).cloned().collect();

        let added = all_names.len();
        let previous = running_count.fetch_add(added, Ordering::Relaxed);
        let new_total = previous.saturating_add(added);
        let _ = progress_tx.send(DiffProgressMsg::Counting(new_total));

        for name in all_names {
            if cancel_flag.load(Ordering::Relaxed) {
                break;
            }
            if left_set.contains(name.as_str())
                && right_set.contains(name.as_str())
                && is_dir_via_info(&left_dir.join(&name))
                && is_dir_via_info(&right_dir.join(&name))
            {
                pending.push(if relative.is_empty() {
                    name
                } else {
                    format!("{}/{}", relative, name)
                });
            }
        }
    }

    running_count.load(Ordering::Relaxed)
}

/// Threaded version of build_recursive with cancel_flag and progress reporting
fn build_recursive_threaded(
    left_root: &Path,
    right_root: &Path,
    left_root_authorization: Option<&file_ops::DirectoryAuthorization>,
    right_root_authorization: Option<&file_ops::DirectoryAuthorization>,
    relative_path: &str,
    depth: usize,
    compare_method: CompareMethod,
    sort_by: SortBy,
    sort_order: SortOrder,
    entries: &mut Vec<DiffEntry>,
    cancel_flag: &AtomicBool,
    progress_tx: &Sender<DiffProgressMsg>,
    total: usize,
    counter: &Arc<std::sync::atomic::AtomicUsize>,
) {
    let left = if relative_path.is_empty() {
        left_root.to_path_buf()
    } else {
        left_root.join(relative_path)
    };
    let right = if relative_path.is_empty() {
        right_root.to_path_buf()
    } else {
        right_root.join(relative_path)
    };
    build_iterative(
        &left,
        &right,
        left_root_authorization,
        right_root_authorization,
        compare_method,
        sort_by,
        sort_order,
        entries,
        Some(BuildProgress {
            cancel_flag,
            progress_tx,
            total,
            counter,
        }),
    );
    if depth != 0 {
        for entry in entries.iter_mut() {
            entry.depth = entry.depth.saturating_add(depth);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helper functions
// ═══════════════════════════════════════════════════════════════════════════════

/// Sort names for lazy-loaded one-side directories (directories first, then by name)
fn sort_names_one_side(names: &mut Vec<String>, dir: &Path) {
    names.sort_by(|a, b| {
        let a_is_dir = dir.join(a).is_dir();
        let b_is_dir = dir.join(b).is_dir();
        match (a_is_dir, b_is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.to_lowercase().cmp(&b.to_lowercase()),
        }
    });
}

/// Read directory entry names, returning an empty vec on failure
fn read_dir_names(dir: &Path) -> Vec<String> {
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn read_dir_names_checked(dir: &Path) -> io::Result<Vec<String>> {
    fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().to_string()))
        .collect()
}

/// Build DiffFileInfo from a path, returning None if the path doesn't exist
fn make_file_info(path: &Path, name: &str) -> Option<DiffFileInfo> {
    let initial_authorization = file_ops::capture_path_authorization(path).ok();
    let metadata = fs::symlink_metadata(path).ok()?;
    let is_symlink = metadata.file_type().is_symlink();
    let actual_metadata = if is_symlink {
        fs::metadata(path).unwrap_or(metadata.clone())
    } else {
        metadata.clone()
    };
    // Never treat a symlink as a directory to recurse into: following symlinked
    // directories can loop forever on a cycle (e.g. `ln -s .. loop`), which makes the
    // recursive tree builder and expand-all recurse without bound. Symlinks are shown
    // as leaf entries (is_symlink distinguishes them) and are not descended into.
    let is_directory = !is_symlink && actual_metadata.is_dir();
    let size = if is_directory {
        0
    } else {
        actual_metadata.len()
    };
    let modified = metadata
        .modified()
        .ok()
        .map(DateTime::<Local>::from)
        .unwrap_or_else(Local::now);
    let authorization = initial_authorization.filter(|expected| {
        file_ops::capture_path_authorization(path).ok().as_ref() == Some(expected)
    });

    Some(DiffFileInfo {
        name: name.to_string(),
        size,
        modified,
        is_directory,
        is_symlink,
        full_path: path.to_path_buf(),
        authorization,
    })
}

/// Sort names: directories first, then by sort_by/sort_order
fn sort_names(
    names: &mut Vec<String>,
    left_dir: &Path,
    right_dir: &Path,
    left_set: &HashSet<&str>,
    right_set: &HashSet<&str>,
    sort_by: SortBy,
    sort_order: SortOrder,
) {
    names.sort_by(|a, b| {
        // Determine if each name is a directory (check both sides)
        let a_is_dir = is_name_dir(a, left_dir, right_dir, left_set, right_set);
        let b_is_dir = is_name_dir(b, left_dir, right_dir, left_set, right_set);

        // Directories first
        match (a_is_dir, b_is_dir) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            _ => {}
        }

        let ord = match sort_by {
            SortBy::Name => a.to_lowercase().cmp(&b.to_lowercase()),
            SortBy::Size => {
                let a_size = get_name_size(a, left_dir, right_dir, left_set, right_set);
                let b_size = get_name_size(b, left_dir, right_dir, left_set, right_set);
                a_size.cmp(&b_size)
            }
            SortBy::Modified => {
                let a_mod = get_name_modified(a, left_dir, right_dir, left_set, right_set);
                let b_mod = get_name_modified(b, left_dir, right_dir, left_set, right_set);
                a_mod.cmp(&b_mod)
            }
            SortBy::Type => {
                let a_ext = get_extension(a);
                let b_ext = get_extension(b);
                a_ext
                    .cmp(&b_ext)
                    .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
            }
        };

        match sort_order {
            SortOrder::Asc => ord,
            SortOrder::Desc => ord.reverse(),
        }
    });
}

fn is_name_dir(
    name: &str,
    left_dir: &Path,
    right_dir: &Path,
    left_set: &HashSet<&str>,
    right_set: &HashSet<&str>,
) -> bool {
    if left_set.contains(name) {
        let path = left_dir.join(name);
        if path.is_dir() {
            return true;
        }
    }
    if right_set.contains(name) {
        let path = right_dir.join(name);
        if path.is_dir() {
            return true;
        }
    }
    false
}

fn get_name_size(
    name: &str,
    left_dir: &Path,
    right_dir: &Path,
    left_set: &HashSet<&str>,
    right_set: &HashSet<&str>,
) -> u64 {
    // Prefer left side for sorting
    if left_set.contains(name) {
        if let Ok(m) = fs::metadata(left_dir.join(name)) {
            return m.len();
        }
    }
    if right_set.contains(name) {
        if let Ok(m) = fs::metadata(right_dir.join(name)) {
            return m.len();
        }
    }
    0
}

fn get_name_modified(
    name: &str,
    left_dir: &Path,
    right_dir: &Path,
    left_set: &HashSet<&str>,
    right_set: &HashSet<&str>,
) -> std::time::SystemTime {
    if left_set.contains(name) {
        if let Ok(m) = fs::metadata(left_dir.join(name)) {
            if let Ok(t) = m.modified() {
                return t;
            }
        }
    }
    if right_set.contains(name) {
        if let Ok(m) = fs::metadata(right_dir.join(name)) {
            if let Ok(t) = m.modified() {
                return t;
            }
        }
    }
    std::time::SystemTime::UNIX_EPOCH
}

fn get_extension(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

// ═══════════════════════════════════════════════════════════════════════════════
// In-memory re-sort (preserving DFS tree structure)
// ═══════════════════════════════════════════════════════════════════════════════

/// Re-sort every sibling list while preserving DFS tree structure, using only
/// index stacks. A directory chain can be much deeper than the process stack.
fn resort_flat_tree(
    entries: &[DiffEntry],
    sort_by: SortBy,
    sort_order: SortOrder,
) -> Vec<DiffEntry> {
    if entries.is_empty() {
        return Vec::new();
    }

    let mut roots = Vec::new();
    let mut children = vec![Vec::<usize>::new(); entries.len()];
    let mut ancestor_stack: Vec<usize> = Vec::new();

    for (index, entry) in entries.iter().enumerate() {
        ancestor_stack.truncate(entry.depth);
        if entry.depth == 0 {
            roots.push(index);
        } else if let Some(&parent) = ancestor_stack.get(entry.depth - 1) {
            children[parent].push(index);
        } else {
            // Keep malformed/incomplete input visible rather than dropping it.
            roots.push(index);
        }
        if entry.is_directory {
            ancestor_stack.push(index);
        }
    }

    let compare = |a: &usize, b: &usize| {
        compare_entries_for_sort(&entries[*a], &entries[*b], sort_by, sort_order)
            .then_with(|| entries[*a].relative_path.cmp(&entries[*b].relative_path))
    };
    roots.sort_by(compare);
    for siblings in &mut children {
        siblings.sort_by(compare);
    }

    let mut result = Vec::with_capacity(entries.len());
    let mut pending: Vec<usize> = roots.into_iter().rev().collect();
    while let Some(index) = pending.pop() {
        result.push(entries[index].clone());
        pending.extend(children[index].iter().rev().copied());
    }
    result
}

/// Compare two DiffEntry items for sorting: directories first, then by sort criteria.
fn compare_entries_for_sort(
    a: &DiffEntry,
    b: &DiffEntry,
    sort_by: SortBy,
    sort_order: SortOrder,
) -> std::cmp::Ordering {
    // Directories first
    match (a.is_directory, b.is_directory) {
        (true, false) => return std::cmp::Ordering::Less,
        (false, true) => return std::cmp::Ordering::Greater,
        _ => {}
    }

    let a_info = a.left.as_ref().or(a.right.as_ref());
    let b_info = b.left.as_ref().or(b.right.as_ref());

    let ord = match sort_by {
        SortBy::Name => {
            let a_name = a_info.map(|i| i.name.to_lowercase()).unwrap_or_default();
            let b_name = b_info.map(|i| i.name.to_lowercase()).unwrap_or_default();
            a_name.cmp(&b_name)
        }
        SortBy::Size => {
            let a_size = a_info.map(|i| i.size).unwrap_or(0);
            let b_size = b_info.map(|i| i.size).unwrap_or(0);
            a_size.cmp(&b_size)
        }
        SortBy::Modified => {
            let a_mod = a_info.map(|i| i.modified);
            let b_mod = b_info.map(|i| i.modified);
            a_mod.cmp(&b_mod)
        }
        SortBy::Type => {
            let a_ext = a_info.map(|i| get_extension(&i.name)).unwrap_or_default();
            let b_ext = b_info.map(|i| get_extension(&i.name)).unwrap_or_default();
            a_ext.cmp(&b_ext).then_with(|| {
                let a_name = a_info.map(|i| i.name.to_lowercase()).unwrap_or_default();
                let b_name = b_info.map(|i| i.name.to_lowercase()).unwrap_or_default();
                a_name.cmp(&b_name)
            })
        }
    };

    match sort_order {
        SortOrder::Asc => ord,
        SortOrder::Desc => ord.reverse(),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// File comparison
// ═══════════════════════════════════════════════════════════════════════════════

/// Compare two files. Returns true if they are considered the same.
pub fn compare_files(left: &DiffFileInfo, right: &DiffFileInfo, method: CompareMethod) -> bool {
    // A symbolic link and a regular file are different filesystem objects even
    // when following the link happens to yield the same bytes.
    if left.is_symlink != right.is_symlink {
        return false;
    }
    // If both are symlinks, compare their target paths
    if left.is_symlink && right.is_symlink {
        return fs::read_link(&left.full_path).ok() == fs::read_link(&right.full_path).ok();
    }
    match method {
        CompareMethod::Content => {
            if left.size != right.size {
                return false;
            }
            byte_compare(&left.full_path, &right.full_path)
        }
        CompareMethod::ModifiedTime => {
            // Compare truncated to seconds to avoid sub-second differences
            left.modified.timestamp() == right.modified.timestamp()
        }
        CompareMethod::ContentAndTime => {
            left.modified.timestamp() == right.modified.timestamp()
                && left.size == right.size
                && byte_compare(&left.full_path, &right.full_path)
        }
    }
}

/// Byte-by-byte comparison of two files using buffered 8KB reads.
/// Returns true if files are identical.
pub fn byte_compare(path_a: &Path, path_b: &Path) -> bool {
    let file_a = match File::open(path_a) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let file_b = match File::open(path_b) {
        Ok(f) => f,
        Err(_) => return false,
    };

    let mut reader_a = BufReader::with_capacity(8192, file_a);
    let mut reader_b = BufReader::with_capacity(8192, file_b);

    const CHUNK_SIZE: usize = 8192;
    let mut buf_a = [0u8; CHUNK_SIZE];
    let mut buf_b = [0u8; CHUNK_SIZE];

    loop {
        let n_a = match read_exact_or_eof(&mut reader_a, &mut buf_a) {
            Ok(n) => n,
            Err(_) => return false,
        };
        let n_b = match read_exact_or_eof(&mut reader_b, &mut buf_b) {
            Ok(n) => n,
            Err(_) => return false,
        };

        if n_a != n_b {
            return false;
        }
        if n_a == 0 {
            return true; // Both files ended
        }
        if buf_a[..n_a] != buf_b[..n_b] {
            return false;
        }
    }
}

/// Read exactly buf.len() bytes, or fewer only at EOF. Returns bytes read.
fn read_exact_or_eof(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break, // EOF
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Drawing
// ═══════════════════════════════════════════════════════════════════════════════

/// Draw the diff comparison screen
pub fn draw(
    frame: &mut Frame,
    state: &mut DiffState,
    area: Rect,
    theme: &Theme,
    kb: &crate::keybindings::Keybindings,
    message: Option<&str>,
) {
    // Layout: Header(1) + ColumnHeader(1) + Content(fill) + StatusBar(1) + FunctionBar(1)
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Header
            Constraint::Length(1), // Column header
            Constraint::Min(3),    // Content
            Constraint::Length(1), // Status bar
            Constraint::Length(1), // Function bar
        ])
        .split(area);

    let header_area = layout[0];
    let col_header_area = layout[1];
    let content_area = layout[2];
    let status_area = layout[3];
    let fn_bar_area = layout[4];

    // ── Header ──────────────────────────────────────────────────────────────
    draw_header(frame, state, header_area, theme);

    if state.is_comparing {
        // Clear column header area to prevent stale residue
        frame.render_widget(Paragraph::new(""), col_header_area);

        // ── Progress screen ─────────────────────────────────────────────────
        draw_comparing_progress(frame, state, content_area, theme, kb);

        // Status bar shows comparing status
        let status_text = if state.progress_count == 0 {
            if state.progress_total > 0 {
                format!(" Counting files... ({})", state.progress_total)
            } else {
                " Counting files...".to_string()
            }
        } else {
            format!(
                " Comparing... {}/{}",
                state.progress_count, state.progress_total
            )
        };
        let status_style = Style::default()
            .fg(theme.diff.status_bar_text)
            .bg(theme.diff.status_bar_bg);
        let line = Line::from(vec![Span::styled(
            format!(
                "{:<width$}",
                status_text,
                width = status_area.width as usize
            ),
            status_style,
        )]);
        frame.render_widget(Paragraph::new(line), status_area);

        if let Some(message) = message {
            draw_message_bar(frame, fn_bar_area, theme, message);
        } else {
            // Function bar shows Close key only
            let close_key = kb.diff_screen_first_key(crate::keybindings::DiffScreenAction::Close);
            let fn_line = Line::from(vec![
                Span::styled(
                    close_key.to_string(),
                    Style::default().fg(theme.diff.footer_key),
                ),
                Span::styled(":cancel", Style::default().fg(theme.diff.footer_text)),
            ]);
            frame.render_widget(Paragraph::new(fn_line), fn_bar_area);
        }
        return;
    }

    // ── Column Headers ──────────────────────────────────────────────────────
    draw_column_headers(frame, col_header_area, theme);

    // ── Content (split 50:50, no gap) ──────────────────────────────────────
    let left_width = content_area.width / 2;
    let right_width = content_area.width - left_width;
    let left_area = Rect::new(
        content_area.x,
        content_area.y,
        left_width,
        content_area.height,
    );
    let right_area = Rect::new(
        content_area.x + left_width,
        content_area.y,
        right_width,
        content_area.height,
    );

    let visible_height = content_area.height as usize;
    state.adjust_scroll(visible_height);

    draw_content_side(frame, state, left_area, theme, true);
    draw_content_side(frame, state, right_area, theme, false);

    // ── Scrollbar ───────────────────────────────────────────────────────────
    if state.filtered_indices.len() > visible_height {
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));

        let mut scrollbar_state =
            ScrollbarState::new(state.filtered_indices.len()).position(state.selected_index);

        frame.render_stateful_widget(scrollbar, content_area, &mut scrollbar_state);
    }

    // ── Status Bar ──────────────────────────────────────────────────────────
    draw_status_bar(frame, state, status_area, theme);

    // ── Function Bar ────────────────────────────────────────────────────────
    if let Some(message) = message {
        draw_message_bar(frame, fn_bar_area, theme, message);
    } else {
        draw_function_bar(frame, fn_bar_area, theme, kb);
    }
}

fn draw_message_bar(frame: &mut Frame, area: Rect, theme: &Theme, message: &str) {
    let sanitized: String = message
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let message = truncate_with_ellipsis(&sanitized, area.width.saturating_sub(2) as usize);
    frame.render_widget(
        Paragraph::new(format!(" {message} ")).style(
            Style::default()
                .fg(theme.message.text)
                .add_modifier(Modifier::BOLD),
        ),
        area,
    );
}

fn draw_comparing_progress(
    frame: &mut Frame,
    state: &DiffState,
    area: Rect,
    theme: &Theme,
    kb: &crate::keybindings::Keybindings,
) {
    let center_y = area.y + area.height / 2;

    // Spinner
    let spinner_chars = ['|', '/', '-', '\\'];
    let spinner_idx = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        / 100) as usize
        % spinner_chars.len();
    let spinner = spinner_chars[spinner_idx];

    let is_counting = state.progress_count == 0;

    if is_counting {
        // Phase 1: Counting files — spinner + live count
        let count_text = if state.progress_total > 0 {
            format!("Counting files... ({})", state.progress_total)
        } else {
            "Counting files...".to_string()
        };
        let title_line = Line::from(vec![
            Span::styled(
                format!("{} ", spinner),
                Style::default().fg(theme.diff.progress_spinner),
            ),
            Span::styled(
                count_text,
                Style::default()
                    .fg(theme.diff.header_label)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);

        let title_area = Rect::new(area.x, center_y, area.width, 1);
        frame.render_widget(
            Paragraph::new(title_line).alignment(Alignment::Center),
            title_area,
        );

        // Cancel hint
        if center_y + 2 < area.y + area.height {
            let close_key = kb.diff_screen_first_key(crate::keybindings::DiffScreenAction::Close);
            let hint_line = Line::from(vec![Span::styled(
                format!("Press {} to cancel", close_key),
                Style::default().fg(theme.diff.progress_hint_text),
            )]);
            let hint_area = Rect::new(area.x, center_y + 2, area.width, 1);
            frame.render_widget(
                Paragraph::new(hint_line).alignment(Alignment::Center),
                hint_area,
            );
        }
        return;
    }

    // Phase 2: Comparing with progress bar
    let bar_width = (area.width as usize).min(60).saturating_sub(10);
    let progress_fraction = (state.progress_count as f64) / (state.progress_total as f64);
    let progress_clamped = progress_fraction.min(1.0);
    let filled = (progress_clamped * bar_width as f64) as usize;
    let empty = bar_width.saturating_sub(filled);
    let percent = (progress_clamped * 100.0) as u8;

    let bar_fill = "\u{2588}".repeat(filled);
    let bar_empty = "\u{2591}".repeat(empty);

    // Line 1: "Comparing directories..." with spinner
    let title_line = Line::from(vec![
        Span::styled(
            format!("{} ", spinner),
            Style::default().fg(theme.diff.progress_spinner),
        ),
        Span::styled(
            "Comparing directories...",
            Style::default()
                .fg(theme.diff.header_label)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    if center_y >= area.y + 1 {
        let title_area = Rect::new(area.x, center_y.saturating_sub(2), area.width, 1);
        frame.render_widget(
            Paragraph::new(title_line).alignment(Alignment::Center),
            title_area,
        );
    }

    // Line 2: Progress bar
    let bar_line = Line::from(vec![
        Span::styled(bar_fill, Style::default().fg(theme.diff.progress_bar_fill)),
        Span::styled(
            bar_empty,
            Style::default().fg(theme.diff.progress_bar_empty),
        ),
        Span::styled(
            format!(" {:3}%", percent),
            Style::default().fg(theme.diff.progress_percent_text),
        ),
    ]);

    let bar_area = Rect::new(area.x, center_y, area.width, 1);
    frame.render_widget(
        Paragraph::new(bar_line).alignment(Alignment::Center),
        bar_area,
    );

    // Line 3: Current file being compared
    let max_path_len = (area.width as usize).saturating_sub(6);
    let current_display = if state.progress_current.width() > max_path_len {
        let suffix = crate::utils::format::display_width_suffix(
            &state.progress_current,
            max_path_len.saturating_sub(3),
        );
        format!("...{}", suffix)
    } else {
        state.progress_current.clone()
    };

    let file_line = Line::from(vec![Span::styled(
        current_display,
        Style::default().fg(theme.diff.progress_value_text),
    )]);

    if center_y + 1 < area.y + area.height {
        let file_area = Rect::new(area.x, center_y + 1, area.width, 1);
        frame.render_widget(
            Paragraph::new(file_line).alignment(Alignment::Center),
            file_area,
        );
    }

    // Line 4: Cancel hint
    if center_y + 3 < area.y + area.height {
        let close_key = kb.diff_screen_first_key(crate::keybindings::DiffScreenAction::Close);
        let hint_line = Line::from(vec![Span::styled(
            format!("Press {} to cancel", close_key),
            Style::default().fg(theme.diff.progress_hint_text),
        )]);
        let hint_area = Rect::new(area.x, center_y + 3, area.width, 1);
        frame.render_widget(
            Paragraph::new(hint_line).alignment(Alignment::Center),
            hint_area,
        );
    }
}

fn draw_header(frame: &mut Frame, state: &DiffState, area: Rect, theme: &Theme) {
    let max_path_width = (area.width as usize).saturating_sub(12); // "[DIFF] " + " ⟷ "
    let half_width = max_path_width / 2;

    let left_str = state.left_root.display().to_string();
    let left_display = if left_str.width() > half_width {
        let suffix =
            crate::utils::format::display_width_suffix(&left_str, half_width.saturating_sub(3));
        format!("...{}", suffix)
    } else {
        left_str
    };

    let right_str = state.right_root.display().to_string();
    let right_display = if right_str.width() > half_width {
        let suffix =
            crate::utils::format::display_width_suffix(&right_str, half_width.saturating_sub(3));
        format!("...{}", suffix)
    } else {
        right_str
    };

    let header_line = Line::from(vec![
        Span::styled(
            "[DIFF] ",
            Style::default()
                .fg(theme.diff.header_label)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(left_display, Style::default().fg(theme.diff.header_text)),
        Span::styled(
            " \u{27F7} ",
            Style::default()
                .fg(theme.diff.header_label)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(right_display, Style::default().fg(theme.diff.header_text)),
    ]);

    frame.render_widget(Paragraph::new(header_line), area);
}

fn draw_column_headers(frame: &mut Frame, area: Rect, theme: &Theme) {
    let left_width = area.width / 2;
    let right_width = area.width - left_width;
    let col_style = Style::default()
        .fg(theme.diff.column_header_text)
        .bg(theme.diff.column_header_bg)
        .add_modifier(Modifier::BOLD);

    // Calculate column widths for each half: Name(fill) + Size(10) + Date(12)
    let size_col = 10;
    let date_col = 12;

    let build_header = |width: u16| -> String {
        let w = width as usize;
        let name_col = w.saturating_sub(size_col + date_col + 3);
        let header = format!(
            " {:<name_w$} {:>size_w$} {:>date_w$}",
            "Name",
            "Size",
            "Date",
            name_w = name_col,
            size_w = size_col,
            date_w = date_col,
        );
        if header.width() > w {
            let s = safe_suffix(&header, w.saturating_sub(3));
            format!("...{}", s)
        } else {
            format!("{:<width$}", header, width = w)
        }
    };

    let header_left = build_header(left_width.saturating_sub(1));
    let header_right = build_header(right_width);

    let line = Line::from(vec![
        Span::styled(header_left, col_style),
        Span::styled(" ", col_style),
        Span::styled(header_right, col_style),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

fn draw_content_side(
    frame: &mut Frame,
    state: &DiffState,
    area: Rect,
    theme: &Theme,
    is_left: bool,
) {
    let visible_height = area.height as usize;
    let width = area.width as usize;

    // Column layout within each side
    let size_col = 10;
    let date_col = 12;
    let name_col = width.saturating_sub(size_col + date_col + 2);

    let mut lines: Vec<Line> = Vec::new();

    let visible_indices: Vec<usize> = state
        .filtered_indices
        .iter()
        .skip(state.scroll_offset)
        .take(visible_height)
        .copied()
        .collect();

    for (row, &entry_idx) in visible_indices.iter().enumerate() {
        let entry = &state.all_entries[entry_idx];
        let display_index = state.scroll_offset + row;
        let is_selected = display_index == state.selected_index;
        let is_file_selected = state.selected_files.contains(&entry.relative_path);

        let info = if is_left { &entry.left } else { &entry.right };

        // Determine styles based on status
        let (name_style, size_style, date_style) = if is_selected {
            let cursor_bg = match entry.status {
                DiffStatus::Modified | DiffStatus::DirModified => theme.diff.modified_text,
                DiffStatus::LeftOnly | DiffStatus::RightOnly => theme.diff.left_only_text,
                _ => theme.diff.cursor_bg,
            };
            let cursor_style = Style::default().fg(theme.diff.cursor_text).bg(cursor_bg);
            (cursor_style, cursor_style, cursor_style)
        } else if is_file_selected {
            let marked_style = Style::default()
                .fg(theme.diff.marked_text)
                .add_modifier(Modifier::BOLD);
            let (_, ss, ds) = get_entry_styles(entry, info.is_some(), is_left, theme);
            (marked_style, ss, ds)
        } else {
            get_entry_styles(entry, info.is_some(), is_left, theme)
        };

        if let Some(file_info) = info {
            // Indent by depth * 2
            let indent = "  ".repeat(entry.depth);
            let selection_marker = if is_file_selected { "*" } else { " " };

            let display_name = if file_info.is_directory {
                let collapse_indicator = if state.collapsed_dirs.contains(&entry.relative_path) {
                    "\u{25B6}" // ▶ (collapsed)
                } else {
                    "\u{25BC}" // ▼ (expanded)
                };
                format!(
                    "{}{}{} {}/",
                    selection_marker, indent, collapse_indicator, file_info.name
                )
            } else {
                format!("{}{}  {}", selection_marker, indent, file_info.name)
            };

            // Truncate name if too long
            let name_str = if display_name.width() > name_col {
                let suffix = safe_suffix(&display_name, name_col.saturating_sub(3));
                format!("...{}", suffix)
            } else {
                display_name
            };

            let size_str = if file_info.is_directory {
                format!("{:>size_w$}", "<DIR>", size_w = size_col)
            } else {
                format!(
                    "{:>size_w$}",
                    format_size(file_info.size),
                    size_w = size_col
                )
            };

            let date_str = format!("{}", file_info.modified.format("%m-%d %H:%M"));

            let line = Line::from(vec![
                Span::styled(
                    format!(
                        " {:<name_w$}",
                        name_str,
                        name_w = name_col.saturating_sub(1)
                    ),
                    name_style,
                ),
                Span::styled(format!(" {}", size_str), size_style),
                Span::styled(format!(" {}", date_str), date_style),
            ]);

            lines.push(line);
        } else {
            // Empty side - this file/dir only exists on the other side
            let empty_style = if is_selected {
                Style::default()
                    .fg(theme.diff.cursor_text)
                    .bg(theme.diff.cursor_bg)
            } else {
                Style::default()
                    .fg(theme.diff.same_text)
                    .bg(theme.diff.empty_bg)
            };

            let line = Line::from(vec![Span::styled(
                format!("{:<width$}", "", width = width),
                empty_style,
            )]);

            lines.push(line);
        }
    }

    // Fill remaining rows with empty lines
    while lines.len() < visible_height {
        lines.push(Line::from(vec![Span::styled(
            format!("{:<width$}", "", width = width),
            Style::default().bg(theme.diff.bg),
        )]));
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);
}

/// Get styles for an entry based on its diff status
fn get_entry_styles(
    entry: &DiffEntry,
    has_info: bool,
    is_left: bool,
    theme: &Theme,
) -> (Style, Style, Style) {
    let dc = &theme.diff;

    if !has_info {
        // Empty side
        let style = Style::default().fg(dc.same_text).bg(dc.empty_bg);
        return (style, style, style);
    }

    match entry.status {
        DiffStatus::Same => {
            let name_style = Style::default().fg(dc.same_text);
            let size_style = Style::default().fg(dc.size_text);
            let date_style = Style::default().fg(dc.date_text);
            (name_style, size_style, date_style)
        }
        DiffStatus::Modified => {
            let style = Style::default()
                .fg(dc.modified_text)
                .add_modifier(Modifier::BOLD);
            (
                style,
                Style::default().fg(dc.size_text),
                Style::default().fg(dc.date_text),
            )
        }
        DiffStatus::LeftOnly => {
            if is_left {
                let style = Style::default()
                    .fg(dc.left_only_text)
                    .add_modifier(Modifier::BOLD);
                (
                    style,
                    Style::default().fg(dc.size_text),
                    Style::default().fg(dc.date_text),
                )
            } else {
                let style = Style::default().fg(dc.same_text).bg(dc.empty_bg);
                (style, style, style)
            }
        }
        DiffStatus::RightOnly => {
            if !is_left {
                let style = Style::default()
                    .fg(dc.right_only_text)
                    .add_modifier(Modifier::BOLD);
                (
                    style,
                    Style::default().fg(dc.size_text),
                    Style::default().fg(dc.date_text),
                )
            } else {
                let style = Style::default().fg(dc.same_text).bg(dc.empty_bg);
                (style, style, style)
            }
        }
        DiffStatus::DirModified => {
            let style = Style::default()
                .fg(dc.dir_modified_text)
                .add_modifier(Modifier::BOLD);
            (
                style,
                Style::default().fg(dc.size_text),
                Style::default().fg(dc.date_text),
            )
        }
        DiffStatus::DirSame => {
            let name_style = Style::default().fg(dc.dir_same_text);
            let size_style = Style::default().fg(dc.size_text);
            let date_style = Style::default().fg(dc.date_text);
            (name_style, size_style, date_style)
        }
    }
}

fn draw_status_bar(frame: &mut Frame, state: &DiffState, area: Rect, theme: &Theme) {
    let total = state.all_entries.len();
    let diff_count = state
        .all_entries
        .iter()
        .filter(|e| matches!(e.status, DiffStatus::Modified | DiffStatus::DirModified))
        .count();
    let left_count = state
        .all_entries
        .iter()
        .filter(|e| e.status == DiffStatus::LeftOnly)
        .count();
    let right_count = state
        .all_entries
        .iter()
        .filter(|e| e.status == DiffStatus::RightOnly)
        .count();

    let selected_count = state.selected_files.len();
    let sel_str = if selected_count > 0 {
        format!(" | Selected: {}", selected_count)
    } else {
        String::new()
    };

    let status_text = format!(
        " Filter: {} | Compare: {} | Total: {} Different: {} Left: {} Right: {}{}",
        state.filter.display_name(),
        state.compare_method.display_name(),
        total,
        diff_count,
        left_count,
        right_count,
        sel_str,
    );

    let status_style = Style::default()
        .fg(theme.diff.status_bar_text)
        .bg(theme.diff.status_bar_bg);

    let line = Line::from(vec![Span::styled(
        format!("{:<width$}", status_text, width = area.width as usize),
        status_style,
    )]);

    frame.render_widget(Paragraph::new(line), area);
}

fn draw_function_bar(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    kb: &crate::keybindings::Keybindings,
) {
    use crate::keybindings::DiffScreenAction;

    let shortcuts: Vec<(String, &str)> = vec![
        (
            format!(
                "{}/{}",
                kb.diff_screen_first_key(DiffScreenAction::MoveUp),
                kb.diff_screen_first_key(DiffScreenAction::MoveDown)
            ),
            "nav ",
        ),
        (
            kb.diff_screen_first_key(DiffScreenAction::ExpandDir)
                .to_string(),
            ":open ",
        ),
        (
            kb.diff_screen_first_key(DiffScreenAction::CollapseDir)
                .to_string(),
            ":close ",
        ),
        (
            kb.diff_screen_first_key(DiffScreenAction::Open).to_string(),
            ":view ",
        ),
        (
            kb.diff_screen_first_key(DiffScreenAction::CopyEntry)
                .to_string(),
            ":copy ",
        ),
        (
            kb.diff_screen_first_key(DiffScreenAction::DeleteEntry)
                .to_string(),
            ":delete ",
        ),
        (
            kb.diff_screen_first_key(DiffScreenAction::ExpandAll)
                .to_string(),
            ":expand ",
        ),
        (
            kb.diff_screen_first_key(DiffScreenAction::CollapseAll)
                .to_string(),
            ":collapse ",
        ),
        (
            kb.diff_screen_first_key(DiffScreenAction::CycleFilter)
                .to_string(),
            ":filter ",
        ),
        (
            kb.diff_screen_first_key(DiffScreenAction::SortByName)
                .to_string(),
            "ame ",
        ),
        (
            kb.diff_screen_first_key(DiffScreenAction::SortBySize)
                .to_string(),
            "ize ",
        ),
        (
            kb.diff_screen_first_key(DiffScreenAction::SortByDate)
                .to_string(),
            "ate ",
        ),
        (
            kb.diff_screen_first_key(DiffScreenAction::SortByType)
                .to_string(),
            ":type ",
        ),
        (
            kb.diff_screen_first_key(DiffScreenAction::Close)
                .to_string(),
            ":back",
        ),
    ];

    let mut spans: Vec<Span> = Vec::new();
    for (key, label) in &shortcuts {
        spans.push(Span::styled(
            key.clone(),
            Style::default().fg(theme.diff.footer_key),
        ));
        spans.push(Span::styled(
            *label,
            Style::default().fg(theme.diff.footer_text),
        ));
    }

    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), area);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Input handling
// ═══════════════════════════════════════════════════════════════════════════════

/// Handle keyboard input for the diff screen
pub fn handle_input(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    use crate::keybindings::DiffScreenAction;

    // While comparing, only Close action is allowed
    if let Some(ref state) = app.diff_state {
        if state.is_comparing {
            if let Some(DiffScreenAction::Close) =
                app.keybindings.diff_screen_action(code, modifiers)
            {
                if let Some(ref mut state) = app.diff_state {
                    state.cancel();
                }
                app.current_screen = Screen::FilePanel;
                app.diff_state = None;
            }
            return;
        }
    }

    let action = match app.keybindings.diff_screen_action(code, modifiers) {
        Some(a) => a,
        None => return,
    };

    {
        let state = match app.diff_state.as_mut() {
            Some(s) => s,
            None => return,
        };

        match action {
            DiffScreenAction::MoveUp => {
                state.move_cursor(-1);
            }
            DiffScreenAction::MoveDown => {
                state.move_cursor(1);
            }
            DiffScreenAction::ExpandDir => {
                state.expand_one_level();
            }
            DiffScreenAction::CollapseDir => {
                state.collapse_one_level();
            }
            DiffScreenAction::PageUp => {
                let page = state.visible_height.saturating_sub(1).max(1) as i32;
                state.move_cursor(-page);
            }
            DiffScreenAction::PageDown => {
                let page = state.visible_height.saturating_sub(1).max(1) as i32;
                state.move_cursor(page);
            }
            DiffScreenAction::GoHome => {
                state.cursor_to_start();
            }
            DiffScreenAction::GoEnd => {
                state.cursor_to_end();
            }
            DiffScreenAction::ToggleSelect => {
                state.toggle_selection();
            }
            DiffScreenAction::CycleFilter => {
                state.filter = state.filter.next();
                state.apply_filter();
            }
            DiffScreenAction::SortByName => {
                toggle_diff_sort(state, SortBy::Name);
                state.resort_entries();
            }
            DiffScreenAction::SortBySize => {
                toggle_diff_sort(state, SortBy::Size);
                state.resort_entries();
            }
            DiffScreenAction::SortByDate => {
                toggle_diff_sort(state, SortBy::Modified);
                state.resort_entries();
            }
            DiffScreenAction::SortByType => {
                toggle_diff_sort(state, SortBy::Type);
                state.resort_entries();
            }
            DiffScreenAction::ExpandAll => {
                state.expand_all();
            }
            DiffScreenAction::CollapseAll => {
                state.collapse();
            }
            DiffScreenAction::CopyEntry => {
                open_copy_dialog(app);
                return;
            }
            DiffScreenAction::DeleteEntry => {
                open_delete_dialog(app);
                return;
            }
            DiffScreenAction::Open => {
                // Handle Enter: view file diff if current entry is a file
                handle_enter(app);
                return;
            }
            DiffScreenAction::Close => {
                app.current_screen = Screen::FilePanel;
                app.diff_state = None;
                return;
            }
        }
    };
}

fn validated_diff_relative_path(relative_path: &str) -> io::Result<PathBuf> {
    use std::path::Component;

    let path = PathBuf::from(relative_path);
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Selected difference has an invalid relative path",
        ));
    }
    Ok(path)
}

fn capture_diff_item(
    root: &file_ops::DirectoryAuthorization,
    relative_path: &Path,
) -> io::Result<DiffAuthorizedItem> {
    let file_name = relative_path
        .file_name()
        .ok_or_else(|| io::Error::other("Selected difference has no file name"))?;
    let relative_parent = relative_path.parent().unwrap_or_else(|| Path::new(""));
    let parent = file_ops::capture_authorized_relative_directory(root, relative_parent)?;
    let path = parent.resolved_path().join(file_name);
    let item = file_ops::capture_path_authorization(&path)?;
    let tree = if item.is_directory() {
        Some(file_ops::capture_tree_authorization(
            &path,
            &item,
            "Diff selected directory",
        )?)
    } else {
        None
    };
    Ok(DiffAuthorizedItem { parent, item, tree })
}

fn authorized_comparison_roots(
    state: &DiffState,
) -> io::Result<(
    file_ops::DirectoryAuthorization,
    file_ops::DirectoryAuthorization,
)> {
    let left_root = state.left_root_authorization.clone().ok_or_else(|| {
        io::Error::other("Diff left root identity is unavailable; restart the comparison")
    })?;
    let right_root = state.right_root_authorization.clone().ok_or_else(|| {
        io::Error::other("Diff right root identity is unavailable; restart the comparison")
    })?;
    file_ops::verify_directory_authorization(
        &state.left_root,
        &left_root,
        "Diff left comparison root",
    )?;
    file_ops::verify_directory_authorization(
        &state.right_root,
        &right_root,
        "Diff right comparison root",
    )?;
    Ok((left_root, right_root))
}

fn ensure_non_overlapping_diff_roots(
    left_root: &file_ops::DirectoryAuthorization,
    right_root: &file_ops::DirectoryAuthorization,
) -> io::Result<()> {
    let left = left_root.resolved_path();
    let right = right_root.resolved_path();
    if left.starts_with(right) || right.starts_with(left) || left_root.same_object(right_root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Copy and delete are disabled when comparison roots overlap",
        ));
    }
    Ok(())
}

fn ensure_selected_trees_do_not_contain_other_root(
    left_item: Option<&DiffAuthorizedItem>,
    right_item: Option<&DiffAuthorizedItem>,
    left_root: &file_ops::DirectoryAuthorization,
    right_root: &file_ops::DirectoryAuthorization,
) -> io::Result<()> {
    let left_contains_right = left_item
        .and_then(|item| item.tree.as_ref())
        .is_some_and(|tree| tree.contains_directory(right_root));
    let right_contains_left = right_item
        .and_then(|item| item.tree.as_ref())
        .is_some_and(|tree| tree.contains_directory(left_root));
    if left_contains_right || right_contains_left {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Copy and delete are disabled when a selected directory contains the other comparison root",
        ));
    }
    Ok(())
}

fn capture_expected_diff_item(
    root: &file_ops::DirectoryAuthorization,
    relative_path: &Path,
    expected: Option<&DiffFileInfo>,
    side: &str,
) -> io::Result<Option<DiffAuthorizedItem>> {
    let expected_authorization = expected
        .map(|info| {
            info.authorization.ok_or_else(|| {
                io::Error::other(format!(
                    "Diff {side} item identity is unavailable; close and reopen the comparison"
                ))
            })
        })
        .transpose()?;
    let current = match capture_diff_item(root, relative_path) {
        Ok(current) => Some(current),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };

    match (expected_authorization, current) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "Diff {side} item appeared since the comparison; close and reopen it before continuing"
            ),
        )),
        (Some(_), None) => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Diff {side} item disappeared since the comparison; close and reopen it before continuing"
            ),
        )),
        (Some(expected), Some(current)) if expected != current.item => {
            Err(io::Error::other(format!(
                "Diff {side} item changed since the comparison; close and reopen it before continuing"
            )))
        }
        (Some(_), Some(current)) => Ok(Some(current)),
    }
}

fn capture_expected_diff_side(
    root: &file_ops::DirectoryAuthorization,
    relative_path: &Path,
    expected: Option<&DiffFileInfo>,
    expected_missing: Option<&file_ops::MissingPathAuthorization>,
    side: &str,
) -> io::Result<(
    Option<DiffAuthorizedItem>,
    Option<file_ops::MissingPathAuthorization>,
)> {
    if expected.is_some() {
        let item =
            capture_expected_diff_item(root, relative_path, expected, side)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Diff {side} item disappeared since the comparison"),
                )
            })?;
        return Ok((Some(item), None));
    }

    let missing = expected_missing.cloned().ok_or_else(|| {
        io::Error::other(format!(
            "Diff {side} missing-path identity is unavailable; restart the comparison"
        ))
    })?;
    file_ops::verify_missing_path_authorization(&missing, &format!("Diff {side} destination"))?;
    Ok((None, Some(missing)))
}

fn open_copy_dialog(app: &mut App) {
    let Some(entry) = app
        .diff_state
        .as_ref()
        .and_then(DiffState::current_entry)
        .cloned()
    else {
        return;
    };

    let result = (|| -> io::Result<DiffCopyPrompt> {
        let relative_path = validated_diff_relative_path(&entry.relative_path)?;
        let state = app
            .diff_state
            .as_ref()
            .ok_or_else(|| io::Error::other("Diff comparison is no longer available"))?;
        let (left_root, right_root) = authorized_comparison_roots(state)?;
        ensure_non_overlapping_diff_roots(&left_root, &right_root)?;
        let (left_item, left_missing) = capture_expected_diff_side(
            &left_root,
            &relative_path,
            entry.left.as_ref(),
            entry.left_missing.as_ref(),
            "left",
        )?;
        let (right_item, right_missing) = capture_expected_diff_side(
            &right_root,
            &relative_path,
            entry.right.as_ref(),
            entry.right_missing.as_ref(),
            "right",
        )?;
        ensure_selected_trees_do_not_contain_other_root(
            left_item.as_ref(),
            right_item.as_ref(),
            &left_root,
            &right_root,
        )?;

        Ok(DiffCopyPrompt {
            relative_path,
            left_root,
            right_root,
            left_item,
            right_item,
            left_missing,
            right_missing,
        })
    })();

    let message = match result {
        Ok(prompt) => {
            if let Some(state) = app.diff_state.as_mut() {
                state.copy_prompt = Some(prompt);
            }
            String::new()
        }
        Err(error) => {
            if let Some(state) = app.diff_state.as_mut() {
                state.copy_prompt = None;
            }
            format!("Cannot prepare copy: {error}")
        }
    };

    app.dialog = Some(Dialog {
        dialog_type: DialogType::DiffCopy,
        input: entry.relative_path,
        cursor_pos: 0,
        message,
        completion: None,
        selected_button: 1,
        selection: None,
        use_md5: false,
    });
}

fn set_copy_dialog_error(app: &mut App, message: impl Into<String>) {
    if let Some(dialog) = app
        .dialog
        .as_mut()
        .filter(|dialog| dialog.dialog_type == DialogType::DiffCopy)
    {
        dialog.message = message.into();
    }
}

fn cancel_copy_dialog(app: &mut App) {
    if let Some(state) = app.diff_state.as_mut() {
        state.copy_prompt = None;
    }
    app.dialog = None;
}

fn start_diff_copy(app: &mut App, direction: DiffCopyDirection) -> io::Result<()> {
    let prompt = app
        .diff_state
        .as_ref()
        .and_then(|state| state.copy_prompt.clone())
        .ok_or_else(|| io::Error::other("Copy source changed; close the dialog and try again"))?;
    let operation_path = prompt.relative_path.to_string_lossy().into_owned();

    let (source_root, target_root, source_item, target_item, target_missing, source_side) =
        match direction {
            DiffCopyDirection::ToLeft => (
                prompt.right_root,
                prompt.left_root,
                prompt.right_item,
                prompt.left_item,
                prompt.left_missing,
                "right",
            ),
            DiffCopyDirection::ToRight => (
                prompt.left_root,
                prompt.right_root,
                prompt.left_item,
                prompt.right_item,
                prompt.right_missing,
                "left",
            ),
        };
    let source_item = source_item.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("Nothing exists on the {source_side} to copy"),
        )
    })?;

    let file_name = prompt
        .relative_path
        .file_name()
        .ok_or_else(|| io::Error::other("Selected difference has no file name"))?
        .to_os_string();
    file_ops::verify_directory_authorization(
        source_root.resolved_path(),
        &source_root,
        "Diff source root",
    )?;
    file_ops::verify_directory_authorization(
        target_root.resolved_path(),
        &target_root,
        "Diff target root",
    )?;

    let DiffAuthorizedItem {
        parent: source_parent,
        item: source_authorization,
        tree: source_tree,
    } = source_item;
    file_ops::verify_directory_authorization(
        source_parent.resolved_path(),
        &source_parent,
        "Diff source parent",
    )?;
    let source_path = source_parent.resolved_path().join(&file_name);
    file_ops::verify_path_authorization(&source_path, &source_authorization, "Diff copy source")?;
    if let Some(tree) = source_tree.as_ref() {
        file_ops::verify_tree_authorization(&source_path, tree, "Diff copy source")?;
    }

    let (target_parent, overwrite_authorization, destination_tree) = match target_item {
        Some(target_item) => {
            let DiffAuthorizedItem { parent, item, tree } = target_item;
            file_ops::verify_directory_authorization(
                parent.resolved_path(),
                &parent,
                "Diff target parent",
            )?;
            let target_path = parent.resolved_path().join(&file_name);
            file_ops::verify_path_authorization(&target_path, &item, "Diff overwrite destination")?;
            if let Some(tree) = tree.as_ref() {
                file_ops::verify_tree_authorization(
                    &target_path,
                    tree,
                    "Diff overwrite destination",
                )?;
            }
            (parent, Some(item), tree)
        }
        None => {
            let target_missing = target_missing.ok_or_else(|| {
                io::Error::other(
                    "Diff destination absence is no longer authorized; restart the comparison",
                )
            })?;
            (
                file_ops::prepare_authorized_missing_path_parent(
                    &target_missing,
                    "Diff copy destination",
                )?,
                None,
                None,
            )
        }
    };
    let source_dir = source_parent.resolved_path().to_path_buf();
    let target_dir = target_parent.resolved_path().to_path_buf();
    let target_path = target_dir.join(&file_name);
    file_ops::validate_copy_destination(&source_path, &target_path)?;

    let mut files_to_overwrite = HashMap::new();
    if let Some(overwrite_authorization) = overwrite_authorization {
        files_to_overwrite.insert(source_path.clone(), overwrite_authorization);
    }
    let mut source_authorizations = HashMap::new();
    source_authorizations.insert(source_path.clone(), source_authorization);
    let mut source_trees = HashMap::new();
    if let Some(source_tree) = source_tree {
        source_trees.insert(source_path.clone(), source_tree);
    }
    let mut destination_trees = HashMap::new();
    if let Some(destination_tree) = destination_tree {
        destination_trees.insert(source_path, destination_tree);
    }

    let mut progress = FileOperationProgress::new(FileOperationType::Copy);
    progress.is_active = true;
    let cancel_flag = progress.cancel_flag.clone();
    let (tx, rx) = mpsc::channel();
    progress.receiver = Some(rx);

    thread::spawn(move || {
        file_ops::copy_files_with_progress_authorized_trees(
            vec![PathBuf::from(file_name)],
            &source_dir,
            &target_dir,
            files_to_overwrite,
            HashSet::new(),
            Some(target_parent),
            source_authorizations,
            Some(source_parent),
            source_trees,
            destination_trees,
            cancel_flag,
            tx,
        );
    });

    if let Some(state) = app.diff_state.as_mut() {
        state.copy_prompt = None;
        state.copy_in_progress = true;
        state.pending_copy_path = Some(operation_path);
    }
    app.file_operation_progress = Some(progress);
    app.dialog = Some(Dialog {
        dialog_type: DialogType::Progress,
        input: String::new(),
        cursor_pos: 0,
        message: String::new(),
        completion: None,
        selected_button: 0,
        selection: None,
        use_md5: false,
    });
    Ok(())
}

pub(crate) fn handle_copy_dialog_input(
    app: &mut App,
    code: KeyCode,
    _modifiers: KeyModifiers,
) -> bool {
    match code {
        KeyCode::Left => {
            if let Err(error) = start_diff_copy(app, DiffCopyDirection::ToLeft) {
                set_copy_dialog_error(app, error.to_string());
            }
        }
        KeyCode::Right => {
            if let Err(error) = start_diff_copy(app, DiffCopyDirection::ToRight) {
                set_copy_dialog_error(app, error.to_string());
            }
        }
        KeyCode::Up | KeyCode::Down | KeyCode::Enter | KeyCode::Esc => {
            cancel_copy_dialog(app);
        }
        _ => {}
    }
    false
}

fn open_delete_dialog(app: &mut App) {
    let Some(entry) = app
        .diff_state
        .as_ref()
        .and_then(DiffState::current_entry)
        .cloned()
    else {
        return;
    };

    let result = (|| -> io::Result<DiffDeletePrompt> {
        let relative_path = validated_diff_relative_path(&entry.relative_path)?;
        let state = app
            .diff_state
            .as_ref()
            .ok_or_else(|| io::Error::other("Diff comparison is no longer available"))?;
        let (left_root, right_root) = authorized_comparison_roots(state)?;
        ensure_non_overlapping_diff_roots(&left_root, &right_root)?;
        let (left_item, left_missing) = capture_expected_diff_side(
            &left_root,
            &relative_path,
            entry.left.as_ref(),
            entry.left_missing.as_ref(),
            "left",
        )?;
        let (right_item, right_missing) = capture_expected_diff_side(
            &right_root,
            &relative_path,
            entry.right.as_ref(),
            entry.right_missing.as_ref(),
            "right",
        )?;
        if left_item.is_none() && right_item.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "The selected item no longer exists on either side",
            ));
        }
        ensure_selected_trees_do_not_contain_other_root(
            left_item.as_ref(),
            right_item.as_ref(),
            &left_root,
            &right_root,
        )?;

        let contains_directory = left_item
            .as_ref()
            .is_some_and(|item| item.item.is_directory())
            || right_item
                .as_ref()
                .is_some_and(|item| item.item.is_directory());
        Ok(DiffDeletePrompt {
            relative_path,
            left_root,
            right_root,
            left_item,
            right_item,
            left_missing,
            right_missing,
            contains_directory,
        })
    })();

    let message = match result {
        Ok(prompt) => {
            if let Some(state) = app.diff_state.as_mut() {
                state.delete_prompt = Some(prompt);
            }
            String::new()
        }
        Err(error) => {
            if let Some(state) = app.diff_state.as_mut() {
                state.delete_prompt = None;
            }
            format!("Cannot prepare deletion: {error}")
        }
    };

    app.dialog = Some(Dialog {
        dialog_type: DialogType::DiffDelete,
        input: entry.relative_path,
        cursor_pos: 0,
        message,
        completion: None,
        selected_button: 1,
        selection: None,
        use_md5: false,
    });
}

fn set_delete_dialog_error(app: &mut App, message: impl Into<String>) {
    if let Some(dialog) = app
        .dialog
        .as_mut()
        .filter(|dialog| dialog.dialog_type == DialogType::DiffDelete)
    {
        dialog.message = message.into();
    }
}

fn cancel_delete_dialog(app: &mut App) {
    if let Some(state) = app.diff_state.as_mut() {
        state.delete_prompt = None;
    }
    app.dialog = None;
}

fn verify_diff_delete_side_absence(
    item: Option<&DiffAuthorizedItem>,
    missing: Option<&file_ops::MissingPathAuthorization>,
    side: &str,
) -> io::Result<()> {
    if item.is_some() {
        return Ok(());
    }
    let missing = missing.ok_or_else(|| {
        io::Error::other(format!(
            "Diff {side} absence is no longer authorized; restart the comparison"
        ))
    })?;
    file_ops::verify_missing_path_authorization(missing, &format!("Diff {side} delete target"))
}

fn verify_diff_delete_target(target: &PreparedDiffDeleteTarget) -> io::Result<()> {
    file_ops::verify_directory_authorization(
        target.root.resolved_path(),
        &target.root,
        &format!("Diff {} root", target.side),
    )?;
    file_ops::verify_directory_authorization(
        target.parent.resolved_path(),
        &target.parent,
        &format!("Diff {} delete parent", target.side),
    )?;
    file_ops::verify_path_authorization(
        &target.path,
        &target.item,
        &format!("Diff {} delete target", target.side),
    )?;
    if let Some(tree) = target.tree.as_ref() {
        file_ops::verify_tree_authorization(
            &target.path,
            tree,
            &format!("Diff {} delete target", target.side),
        )?;
    }
    Ok(())
}

fn run_diff_delete_worker(
    mut targets: Vec<PreparedDiffDeleteTarget>,
    absence_checks: Vec<(&'static str, file_ops::MissingPathAuthorization)>,
    relative_path: PathBuf,
    cancel_flag: Arc<AtomicBool>,
    tx: Sender<ProgressMessage>,
) {
    let total = targets.len();
    let _ = tx.send(ProgressMessage::TotalProgress(0, total, 0, 0));

    // Repeat absence checks in the worker immediately before the first
    // mutation. The dialog handler checks them too, but the filesystem can
    // change while the worker thread is being scheduled.
    for (side, missing) in &absence_checks {
        if let Err(error) = file_ops::verify_missing_path_authorization(
            missing,
            &format!("Diff {side} delete target"),
        ) {
            let _ = tx.send(ProgressMessage::Error(
                relative_path.display().to_string(),
                format!("{side}: {error}"),
            ));
            let _ = tx.send(ProgressMessage::Completed(0, total));
            return;
        }
    }

    let mut success_count = 0;
    let mut failure_count = 0;
    let mut processed_count = 0;
    let mut errors = Vec::new();

    while processed_count < total {
        if cancel_flag.load(Ordering::Relaxed) {
            failure_count += total.saturating_sub(processed_count);
            errors.push(if success_count == 0 {
                "Cancelled".to_string()
            } else {
                format!(
                    "Cancelled after deleting {success_count}/{total}; completed deletions cannot be undone"
                )
            });
            break;
        }

        let target = targets[processed_count].clone();
        let display_name = format!("{}: {}", target.side, relative_path.display());
        let _ = tx.send(ProgressMessage::FileStarted(display_name.clone()));
        let result = verify_diff_delete_target(&target).and_then(|()| {
            if let Some(tree) = target.tree.as_ref() {
                file_ops::delete_file_detailed_authorized_tree(&target.path, &target.item, tree)
            } else {
                file_ops::delete_file_detailed_authorized(&target.path, &target.item)
            }
        });
        match result {
            Ok(warnings) => {
                success_count += 1;
                // Deleting one hard-link name updates metadata shared by its
                // aliases. Advance only remaining targets that had the exact
                // same approved snapshot; replaced or modified paths remain
                // unauthorized and will fail closed below.
                for pending in targets.iter_mut().skip(processed_count + 1) {
                    file_ops::refresh_path_authorization_after_alias_deletion(
                        &mut pending.item,
                        &target.item,
                        &pending.path,
                    );
                }
                for warning in warnings {
                    let _ = tx.send(ProgressMessage::Warning(display_name.clone(), warning));
                }
            }
            Err(error) => {
                failure_count += 1;
                errors.push(format!("{}: {}", target.side, error));
            }
        }
        processed_count += 1;
        let _ = tx.send(ProgressMessage::TotalProgress(processed_count, total, 0, 0));
    }

    if !errors.is_empty() {
        let _ = tx.send(ProgressMessage::Error(
            relative_path.display().to_string(),
            errors.join("; "),
        ));
    }
    let _ = tx.send(ProgressMessage::Completed(success_count, failure_count));
}

fn start_diff_delete(app: &mut App, direction: DiffDeleteDirection) -> io::Result<()> {
    let prompt = app
        .diff_state
        .as_ref()
        .and_then(|state| state.delete_prompt.clone())
        .ok_or_else(|| {
            io::Error::other("Delete selection changed; close the dialog and try again")
        })?;
    let operation_path = prompt.relative_path.to_string_lossy().into_owned();
    let file_name = prompt
        .relative_path
        .file_name()
        .ok_or_else(|| io::Error::other("Selected difference has no file name"))?
        .to_os_string();

    let make_target = |side: &'static str,
                       root: file_ops::DirectoryAuthorization,
                       selected: Option<DiffAuthorizedItem>|
     -> io::Result<PreparedDiffDeleteTarget> {
        let selected = selected.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Nothing exists on the {side} to delete"),
            )
        })?;
        let path = selected.parent.resolved_path().join(&file_name);
        Ok(PreparedDiffDeleteTarget {
            side,
            root,
            parent: selected.parent,
            path,
            item: selected.item,
            tree: selected.tree,
        })
    };

    let mut targets = Vec::with_capacity(2);
    let mut absence_checks = Vec::with_capacity(1);
    match direction {
        DiffDeleteDirection::Left => {
            verify_diff_delete_side_absence(
                prompt.left_item.as_ref(),
                prompt.left_missing.as_ref(),
                "left",
            )?;
            targets.push(make_target("left", prompt.left_root, prompt.left_item)?)
        }
        DiffDeleteDirection::Right => {
            verify_diff_delete_side_absence(
                prompt.right_item.as_ref(),
                prompt.right_missing.as_ref(),
                "right",
            )?;
            targets.push(make_target("right", prompt.right_root, prompt.right_item)?)
        }
        DiffDeleteDirection::Both => {
            // Revalidate every side, including sides that were absent when the
            // dialog opened, before deleting either existing item.
            verify_diff_delete_side_absence(
                prompt.left_item.as_ref(),
                prompt.left_missing.as_ref(),
                "left",
            )?;
            verify_diff_delete_side_absence(
                prompt.right_item.as_ref(),
                prompt.right_missing.as_ref(),
                "right",
            )?;
            if prompt.left_item.is_none() {
                absence_checks.push((
                    "left",
                    prompt.left_missing.clone().ok_or_else(|| {
                        io::Error::other(
                            "Diff left absence is no longer authorized; restart the comparison",
                        )
                    })?,
                ));
            }
            if prompt.right_item.is_none() {
                absence_checks.push((
                    "right",
                    prompt.right_missing.clone().ok_or_else(|| {
                        io::Error::other(
                            "Diff right absence is no longer authorized; restart the comparison",
                        )
                    })?,
                ));
            }
            if prompt.left_item.is_some() {
                targets.push(make_target("left", prompt.left_root, prompt.left_item)?);
            }
            if prompt.right_item.is_some() {
                targets.push(make_target("right", prompt.right_root, prompt.right_item)?);
            }
            if targets.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "Nothing remains on either side to delete",
                ));
            }
        }
    }

    // Two panels may resolve to the same directory (including through aliases).
    // The single directory entry must be removed only once.
    if targets.len() == 2 && targets[0].path == targets[1].path {
        targets[0].side = "left and right";
        targets.truncate(1);
    }

    // Validate every requested side before mutating either one. The worker
    // repeats these checks because the filesystem can still change afterward.
    for target in &targets {
        verify_diff_delete_target(target)?;
    }

    let mut progress = FileOperationProgress::new(FileOperationType::Delete);
    progress.is_active = true;
    let cancel_flag = progress.cancel_flag.clone();
    let (tx, rx) = mpsc::channel();
    progress.receiver = Some(rx);
    let relative_path = prompt.relative_path;

    thread::spawn(move || {
        run_diff_delete_worker(targets, absence_checks, relative_path, cancel_flag, tx)
    });

    if let Some(state) = app.diff_state.as_mut() {
        state.delete_prompt = None;
        state.delete_in_progress = true;
        state.pending_delete_path = Some(operation_path);
    }
    app.file_operation_progress = Some(progress);
    app.dialog = Some(Dialog {
        dialog_type: DialogType::Progress,
        input: String::new(),
        cursor_pos: 0,
        message: String::new(),
        completion: None,
        selected_button: 0,
        selection: None,
        use_md5: false,
    });
    Ok(())
}

pub(crate) fn handle_delete_dialog_input(
    app: &mut App,
    code: KeyCode,
    _modifiers: KeyModifiers,
) -> bool {
    match code {
        KeyCode::Left => {
            if let Err(error) = start_diff_delete(app, DiffDeleteDirection::Left) {
                set_delete_dialog_error(app, error.to_string());
            }
        }
        KeyCode::Right => {
            if let Err(error) = start_diff_delete(app, DiffDeleteDirection::Right) {
                set_delete_dialog_error(app, error.to_string());
            }
        }
        KeyCode::Down => {
            if let Err(error) = start_diff_delete(app, DiffDeleteDirection::Both) {
                set_delete_dialog_error(app, error.to_string());
            }
        }
        KeyCode::Up | KeyCode::Enter | KeyCode::Esc => {
            cancel_delete_dialog(app);
        }
        _ => {}
    }
    false
}

/// Toggle sort field/order for the diff state
fn toggle_diff_sort(state: &mut DiffState, sort_by: SortBy) {
    if state.sort_by == sort_by {
        state.sort_order = match state.sort_order {
            SortOrder::Asc => SortOrder::Desc,
            SortOrder::Desc => SortOrder::Asc,
        };
    } else {
        state.sort_by = sort_by;
        state.sort_order = SortOrder::Asc;
    }
    state.selected_index = 0;
    state.scroll_offset = 0;
}

/// Handle Enter key - toggle directory collapse or open file content diff view
fn handle_enter(app: &mut App) {
    let entry = {
        let state = match app.diff_state.as_ref() {
            Some(s) => s,
            None => return,
        };
        match state.current_entry() {
            Some(e) => e.clone(),
            None => return,
        }
    };

    if entry.is_directory {
        // Toggle collapse/expand for directories
        if let Some(ref mut state) = app.diff_state {
            state.toggle_collapse();
        }
        return;
    }

    // Need both sides for file diff view
    let left_path = entry
        .left
        .as_ref()
        .map(|f| f.full_path.clone())
        .unwrap_or_default();
    let right_path = entry
        .right
        .as_ref()
        .map(|f| f.full_path.clone())
        .unwrap_or_default();

    // Get file name for display
    let file_name = entry.relative_path.clone();

    // Enter file content diff view
    app.enter_diff_file_view(left_path, right_path, file_name);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use std::time::{Duration, Instant};

    fn app_with_diff(left: &Path, right: &Path) -> App {
        let mut app = App::new(left.to_path_buf(), right.to_path_buf());
        let mut state = DiffState::new(
            left.to_path_buf(),
            right.to_path_buf(),
            CompareMethod::Content,
            SortBy::Name,
            SortOrder::Asc,
        );
        state.filter = DiffFilter::All;
        state.build_diff_list();
        state.apply_filter();
        state.expand_all();
        app.current_screen = Screen::DiffScreen;
        app.diff_state = Some(state);
        app
    }

    fn select_entry(app: &mut App, relative_path: &str) {
        let state = app.diff_state.as_mut().unwrap();
        state.selected_index = state
            .filtered_indices
            .iter()
            .position(|&index| state.all_entries[index].relative_path == relative_path)
            .unwrap_or_else(|| panic!("missing diff entry: {relative_path}"));
    }

    fn open_copy_prompt(app: &mut App) {
        handle_input(app, KeyCode::Char('C'), KeyModifiers::SHIFT);
        assert_eq!(
            app.dialog.as_ref().map(|dialog| dialog.dialog_type),
            Some(DialogType::DiffCopy)
        );
    }

    fn open_delete_prompt(app: &mut App) {
        handle_input(app, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(
            app.dialog.as_ref().map(|dialog| dialog.dialog_type),
            Some(DialogType::DiffDelete)
        );
    }

    fn wait_for_copy(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let active = app
                .file_operation_progress
                .as_mut()
                .expect("copy progress should exist")
                .poll();
            if !active {
                break;
            }
            assert!(Instant::now() < deadline, "copy worker timed out");
            std::thread::sleep(Duration::from_millis(1));
        }

        let result = app
            .file_operation_progress
            .as_ref()
            .and_then(|progress| progress.result.as_ref())
            .expect("copy result should exist");
        assert_eq!(
            result.success_count, 1,
            "copy error: {:?}",
            result.last_error
        );
        assert_eq!(
            result.failure_count, 0,
            "copy error: {:?}",
            result.last_error
        );
    }

    fn wait_for_delete(app: &mut App) -> file_ops::FileOperationResult {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let active = app
                .file_operation_progress
                .as_mut()
                .expect("delete progress should exist")
                .poll();
            if !active {
                break;
            }
            assert!(Instant::now() < deadline, "delete worker timed out");
            std::thread::sleep(Duration::from_millis(1));
        }

        app.file_operation_progress
            .as_ref()
            .and_then(|progress| progress.result.clone())
            .expect("delete result should exist")
    }

    #[test]
    fn read_error_is_not_reported_as_eof() {
        struct BrokenReader;
        impl Read for BrokenReader {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::Other, "broken"))
            }
        }

        let mut reader = BrokenReader;
        let mut buffer = [0u8; 8];
        assert!(read_exact_or_eof(&mut reader, &mut buffer).is_err());
    }

    #[test]
    fn function_bar_exposes_diff_copy_and_delete_at_80_columns() {
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();
        let keybindings = crate::keybindings::Keybindings::from_config(
            &crate::keybindings::KeybindingsConfig::default(),
        );

        terminal
            .draw(|frame| {
                draw_function_bar(frame, frame.area(), &theme, &keybindings);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let row = (0..80)
            .map(|x| buffer.cell((x, 0)).unwrap().symbol())
            .collect::<String>();
        assert!(row.contains("Shift+C:copy"), "got: {row:?}");
        assert!(row.contains("Del:delete"), "got: {row:?}");
    }

    #[test]
    fn comparison_progress_displays_an_operation_result_message() {
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();
        let keybindings = crate::keybindings::Keybindings::from_config(
            &crate::keybindings::KeybindingsConfig::default(),
        );
        let mut state = DiffState::new(
            PathBuf::from("left"),
            PathBuf::from("right"),
            CompareMethod::Content,
            SortBy::Name,
            SortOrder::Asc,
        );
        state.is_comparing = true;

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut state,
                    frame.area(),
                    &theme,
                    &keybindings,
                    Some("Copy completed"),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let row = (0..80)
            .map(|x| buffer.cell((x, 9)).unwrap().symbol())
            .collect::<String>();
        assert!(row.contains("Copy completed"), "got: {row:?}");
    }

    #[test]
    fn completed_comparison_prunes_only_stale_selections() {
        let mut state = DiffState::new(
            PathBuf::from("left"),
            PathBuf::from("right"),
            CompareMethod::Content,
            SortBy::Name,
            SortOrder::Asc,
        );
        state.selected_files.insert("still-present".to_string());
        state.selected_files.insert("removed".to_string());
        let entry = DiffEntry {
            relative_path: "still-present".to_string(),
            left: None,
            right: None,
            status: DiffStatus::LeftOnly,
            is_directory: false,
            depth: 0,
            children_not_loaded: false,
            left_missing: None,
            right_missing: None,
        };
        let (sender, receiver) = mpsc::channel();
        sender.send(DiffCompareResult(vec![entry])).unwrap();
        state.receiver = Some(receiver);
        state.is_comparing = true;

        assert!(state.poll());
        assert_eq!(state.selected_files.len(), 1);
        assert!(state.selected_files.contains("still-present"));
    }

    #[test]
    fn copy_completion_reconciles_only_the_operated_path() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        std::fs::write(left.path().join("target.txt"), b"target").unwrap();
        std::fs::write(left.path().join("unrelated.txt"), b"unrelated").unwrap();
        let mut state = DiffState::new(
            left.path().to_path_buf(),
            right.path().to_path_buf(),
            CompareMethod::Content,
            SortBy::Name,
            SortOrder::Asc,
        );
        state.filter = DiffFilter::All;
        state.build_diff_list();
        state.apply_filter();

        std::fs::write(right.path().join("target.txt"), b"target").unwrap();
        // This simulates an unrelated external change. A local reconciliation
        // must deliberately leave that comparison snapshot alone.
        std::fs::write(right.path().join("unrelated.txt"), b"unrelated").unwrap();
        state.copy_in_progress = true;
        state.pending_copy_path = Some("target.txt".to_string());

        state.finish_copy_operation().unwrap();

        assert!(!state.is_comparing);
        assert!(!state.copy_in_progress());
        assert_eq!(
            state
                .all_entries
                .iter()
                .find(|entry| entry.relative_path == "target.txt")
                .map(|entry| entry.status),
            Some(DiffStatus::Same)
        );
        assert_eq!(
            state
                .all_entries
                .iter()
                .find(|entry| entry.relative_path == "unrelated.txt")
                .map(|entry| entry.status),
            Some(DiffStatus::LeftOnly)
        );
    }

    #[test]
    fn nested_copy_reconciles_the_created_ancestor_side() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(left.path().join("parent/child")).unwrap();
        std::fs::write(left.path().join("parent/child/target.txt"), b"target").unwrap();
        let mut state = DiffState::new(
            left.path().to_path_buf(),
            right.path().to_path_buf(),
            CompareMethod::Content,
            SortBy::Name,
            SortOrder::Asc,
        );
        state.filter = DiffFilter::All;
        state.build_diff_list();
        state.apply_filter();
        state.expand_all();

        std::fs::create_dir_all(right.path().join("parent/child")).unwrap();
        std::fs::write(right.path().join("parent/child/target.txt"), b"target").unwrap();
        state.copy_in_progress = true;
        state.pending_copy_path = Some("parent/child/target.txt".to_string());
        state.finish_copy_operation().unwrap();

        for path in ["parent", "parent/child"] {
            let entry = state
                .all_entries
                .iter()
                .find(|entry| entry.relative_path == path)
                .unwrap();
            assert!(entry.left.is_some());
            assert!(entry.right.is_some());
            assert_eq!(entry.status, DiffStatus::DirSame);
            assert!(!state.collapsed_dirs.contains(path));
        }
        assert_eq!(
            state
                .all_entries
                .iter()
                .find(|entry| entry.relative_path == "parent/child/target.txt")
                .map(|entry| entry.status),
            Some(DiffStatus::Same)
        );
    }

    #[test]
    fn targeted_reconciliation_preserves_expansion_and_unrelated_state() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(left.path().join("dir/expanded")).unwrap();
        std::fs::create_dir_all(left.path().join("dir/collapsed")).unwrap();
        std::fs::write(left.path().join("dir/expanded/file.txt"), b"expanded").unwrap();
        std::fs::write(left.path().join("dir/collapsed/file.txt"), b"collapsed").unwrap();

        let mut state = DiffState::new(
            left.path().to_path_buf(),
            right.path().to_path_buf(),
            CompareMethod::Content,
            SortBy::Name,
            SortOrder::Asc,
        );
        state.filter = DiffFilter::All;
        state.build_diff_list();
        state.apply_filter();
        state.expand_all();
        state.collapsed_dirs.insert("dir/collapsed".to_string());
        state.apply_filter();
        state
            .selected_files
            .insert("dir/expanded/file.txt".to_string());
        state
            .selected_files
            .insert("unrelated-selection.txt".to_string());

        // A completed attempt is reconciled even when the worker failed. An
        // unrelated external addition must not leak into that local update.
        std::fs::create_dir_all(left.path().join("unrelated-new")).unwrap();

        state.copy_in_progress = true;
        state.pending_copy_path = Some("dir".to_string());
        state.finish_copy_operation().unwrap();

        assert!(!state.collapsed_dirs.contains("dir"));
        assert!(!state.collapsed_dirs.contains("dir/expanded"));
        assert!(state.collapsed_dirs.contains("dir/collapsed"));
        assert!(state.selected_files.contains("dir/expanded/file.txt"));
        assert!(state.selected_files.contains("unrelated-selection.txt"));
        assert!(!state
            .all_entries
            .iter()
            .any(|entry| entry.relative_path == "unrelated-new"));

        let visible_paths: Vec<_> = state
            .filtered_indices
            .iter()
            .map(|&index| state.all_entries[index].relative_path.as_str())
            .collect();
        assert!(visible_paths.contains(&"dir/expanded/file.txt"));
        assert!(!visible_paths.contains(&"dir/collapsed/file.txt"));
    }

    #[test]
    fn cursor_moves_to_the_next_surviving_row_and_keeps_its_visual_offset() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            std::fs::write(left.path().join(name), name.as_bytes()).unwrap();
        }
        let mut state = DiffState::new(
            left.path().to_path_buf(),
            right.path().to_path_buf(),
            CompareMethod::Content,
            SortBy::Name,
            SortOrder::Asc,
        );
        state.build_diff_list();
        state.apply_filter();
        state.visible_height = 2;
        state.selected_index = 2;
        state.scroll_offset = 1;
        assert_eq!(
            state
                .current_entry()
                .map(|entry| entry.relative_path.as_str()),
            Some("c.txt")
        );

        std::fs::write(right.path().join("c.txt"), b"c.txt").unwrap();
        state.copy_in_progress = true;
        state.pending_copy_path = Some("c.txt".to_string());
        state.finish_copy_operation().unwrap();

        assert_eq!(
            state
                .current_entry()
                .map(|entry| entry.relative_path.as_str()),
            Some("d.txt")
        );
        assert_eq!(state.selected_index.saturating_sub(state.scroll_offset), 1);
        assert!(!state.is_comparing);
    }

    #[test]
    fn partial_delete_keeps_the_cursor_on_the_still_visible_target() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        std::fs::write(left.path().join("target.txt"), b"left").unwrap();
        std::fs::write(right.path().join("target.txt"), b"right").unwrap();
        let mut state = DiffState::new(
            left.path().to_path_buf(),
            right.path().to_path_buf(),
            CompareMethod::Content,
            SortBy::Name,
            SortOrder::Asc,
        );
        state.build_diff_list();
        state.apply_filter();

        std::fs::remove_file(left.path().join("target.txt")).unwrap();
        state.delete_in_progress = true;
        state.pending_delete_path = Some("target.txt".to_string());
        state.finish_delete_operation().unwrap();

        let current = state.current_entry().unwrap();
        assert_eq!(current.relative_path, "target.txt");
        assert_eq!(current.status, DiffStatus::RightOnly);
        assert!(!state.delete_in_progress());
        assert!(!state.is_comparing);
    }

    #[test]
    fn two_sided_delete_removes_only_the_target_entry_and_its_selection() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        std::fs::create_dir(left.path().join("target")).unwrap();
        std::fs::create_dir(right.path().join("target")).unwrap();
        std::fs::write(left.path().join("target/child.txt"), b"same").unwrap();
        std::fs::write(right.path().join("target/child.txt"), b"same").unwrap();
        std::fs::write(left.path().join("unrelated.txt"), b"left").unwrap();
        let mut state = DiffState::new(
            left.path().to_path_buf(),
            right.path().to_path_buf(),
            CompareMethod::Content,
            SortBy::Name,
            SortOrder::Asc,
        );
        state.filter = DiffFilter::All;
        state.build_diff_list();
        state.apply_filter();
        state.selected_files.insert("target".to_string());
        state.selected_files.insert("target/child.txt".to_string());
        state.selected_files.insert("unrelated.txt".to_string());

        std::fs::remove_dir_all(left.path().join("target")).unwrap();
        std::fs::remove_dir_all(right.path().join("target")).unwrap();
        state.delete_in_progress = true;
        state.pending_delete_path = Some("target".to_string());
        state.finish_delete_operation().unwrap();

        assert!(!state
            .all_entries
            .iter()
            .any(|entry| diff_path_is_within(&entry.relative_path, "target")));
        assert!(!state.selected_files.contains("target"));
        assert!(!state.selected_files.contains("target/child.txt"));
        assert!(state.selected_files.contains("unrelated.txt"));
        assert_eq!(
            state
                .current_entry()
                .map(|entry| entry.relative_path.as_str()),
            Some("unrelated.txt")
        );
    }

    #[test]
    fn targeted_reconciliation_rejects_a_replaced_root() {
        let workspace = tempfile::tempdir().unwrap();
        let left = workspace.path().join("left");
        let displaced_left = workspace.path().join("left-old");
        let right = workspace.path().join("right");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("target.txt"), b"target").unwrap();
        let mut state = DiffState::new(
            left.clone(),
            right,
            CompareMethod::Content,
            SortBy::Name,
            SortOrder::Asc,
        );
        state.build_diff_list();
        state.apply_filter();
        let previous_entries = state.all_entries.clone();

        std::fs::rename(&left, &displaced_left).unwrap();
        std::fs::create_dir(&left).unwrap();
        state.delete_in_progress = true;
        state.pending_delete_path = Some("target.txt".to_string());

        let error = state.finish_delete_operation().unwrap_err();

        assert!(error.to_string().contains("root was replaced"));
        assert_eq!(state.all_entries.len(), previous_entries.len());
        assert_eq!(state.all_entries[0].relative_path, "target.txt");
        assert!(state.left_root_authorization.is_none());
        assert!(state.right_root_authorization.is_none());
        assert!(!state.delete_in_progress());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_regular_file_are_not_equal_even_with_same_bytes() {
        let temp_dir = tempfile::tempdir().unwrap();
        let regular = temp_dir.path().join("regular");
        let link = temp_dir.path().join("link");
        std::fs::write(&regular, b"same bytes").unwrap();
        std::os::unix::fs::symlink(&regular, &link).unwrap();
        let left = make_file_info(&link, "link").unwrap();
        let right = make_file_info(&regular, "regular").unwrap();

        assert!(!compare_files(&left, &right, CompareMethod::Content));
    }

    #[test]
    fn deeply_nested_comparison_uses_an_explicit_stack() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        let mut left_cursor = left.path().to_path_buf();
        let mut right_cursor = right.path().to_path_buf();
        const DEPTH: usize = 700;
        for _ in 0..DEPTH {
            left_cursor.push("d");
            right_cursor.push("d");
            std::fs::create_dir(&left_cursor).unwrap();
            std::fs::create_dir(&right_cursor).unwrap();
        }
        std::fs::write(left_cursor.join("leaf"), b"same").unwrap();
        std::fs::write(right_cursor.join("leaf"), b"same").unwrap();

        let mut state = DiffState::new(
            left.path().to_path_buf(),
            right.path().to_path_buf(),
            CompareMethod::Content,
            SortBy::Name,
            SortOrder::Asc,
        );
        state.build_diff_list();

        assert_eq!(state.all_entries.len(), DEPTH + 1);
        assert_eq!(state.all_entries.last().unwrap().status, DiffStatus::Same);
        assert_eq!(state.all_entries.last().unwrap().depth, DEPTH);
    }

    #[test]
    fn deeply_nested_resort_uses_an_explicit_stack() {
        const DEPTH: usize = 20_000;
        let entries: Vec<DiffEntry> = (0..DEPTH)
            .map(|depth| DiffEntry {
                relative_path: depth.to_string(),
                left: None,
                right: None,
                status: DiffStatus::DirSame,
                is_directory: true,
                depth,
                children_not_loaded: false,
                left_missing: None,
                right_missing: None,
            })
            .collect();

        let sorted = resort_flat_tree(&entries, SortBy::Name, SortOrder::Asc);

        assert_eq!(sorted.len(), DEPTH);
        assert_eq!(sorted.last().unwrap().depth, DEPTH - 1);
    }

    #[test]
    fn iterative_resort_keeps_children_with_their_parent() {
        let make_entry = |path: &str, name: &str, depth: usize| DiffEntry {
            relative_path: path.to_string(),
            left: Some(DiffFileInfo {
                name: name.to_string(),
                size: 0,
                modified: Local::now(),
                is_directory: true,
                is_symlink: false,
                full_path: PathBuf::from(path),
                authorization: None,
            }),
            right: None,
            status: DiffStatus::LeftOnly,
            is_directory: true,
            depth,
            children_not_loaded: false,
            left_missing: None,
            right_missing: None,
        };
        let entries = vec![
            make_entry("b", "b", 0),
            make_entry("b/child", "child-b", 1),
            make_entry("a", "a", 0),
            make_entry("a/child", "child-a", 1),
        ];

        let sorted = resort_flat_tree(&entries, SortBy::Name, SortOrder::Asc);
        let paths: Vec<&str> = sorted
            .iter()
            .map(|entry| entry.relative_path.as_str())
            .collect();

        assert_eq!(paths, ["a", "a/child", "b", "b/child"]);
    }

    #[test]
    fn left_on_file_focuses_and_collapses_its_parent() {
        let entry = |relative_path: &str, is_directory: bool, depth: usize| DiffEntry {
            relative_path: relative_path.to_string(),
            left: None,
            right: None,
            status: if is_directory {
                DiffStatus::DirModified
            } else {
                DiffStatus::Modified
            },
            is_directory,
            depth,
            children_not_loaded: false,
            left_missing: None,
            right_missing: None,
        };
        let mut state = DiffState::new(
            PathBuf::from("left"),
            PathBuf::from("right"),
            CompareMethod::Content,
            SortBy::Name,
            SortOrder::Asc,
        );
        state.filter = DiffFilter::All;
        state.all_entries = vec![
            entry("src", true, 0),
            entry("src/ui", true, 1),
            entry("src/ui/help.rs", false, 2),
            entry("src/ui/nested", true, 2),
            entry("src/ui/nested/view.rs", false, 3),
            entry("src/app.rs", false, 1),
        ];
        state.apply_filter();
        state.selected_index = state
            .filtered_indices
            .iter()
            .position(|&index| state.all_entries[index].relative_path == "src/ui/help.rs")
            .unwrap();

        state.collapse_one_level();

        assert_eq!(
            state
                .current_entry()
                .map(|entry| entry.relative_path.as_str()),
            Some("src/ui")
        );
        assert!(state.collapsed_dirs.contains("src/ui"));
        assert!(state.collapsed_dirs.contains("src/ui/nested"));
        let visible_paths: Vec<&str> = state
            .filtered_indices
            .iter()
            .map(|&index| state.all_entries[index].relative_path.as_str())
            .collect();
        assert_eq!(visible_paths, ["src", "src/ui", "src/app.rs"]);
    }

    #[test]
    fn diff_copy_arrows_overwrite_files_in_both_directions() {
        for copy_to_right in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let left = temp.path().join("left");
            let right = temp.path().join("right");
            std::fs::create_dir(&left).unwrap();
            std::fs::create_dir(&right).unwrap();
            std::fs::write(left.join("item.txt"), b"left version").unwrap();
            std::fs::write(right.join("item.txt"), b"right version").unwrap();

            let mut app = app_with_diff(&left, &right);
            select_entry(&mut app, "item.txt");
            open_copy_prompt(&mut app);
            crate::ui::dialogs::handle_dialog_input(
                &mut app,
                if copy_to_right {
                    KeyCode::Right
                } else {
                    KeyCode::Left
                },
                KeyModifiers::NONE,
            );
            assert_eq!(
                app.dialog.as_ref().map(|dialog| dialog.dialog_type),
                Some(DialogType::Progress)
            );
            wait_for_copy(&mut app);

            if copy_to_right {
                assert_eq!(
                    std::fs::read(right.join("item.txt")).unwrap(),
                    b"left version"
                );
            } else {
                assert_eq!(
                    std::fs::read(left.join("item.txt")).unwrap(),
                    b"right version"
                );
            }
        }
    }

    #[test]
    fn diff_copy_replaces_an_existing_directory() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir_all(left.join("folder/nested")).unwrap();
        std::fs::create_dir_all(right.join("folder")).unwrap();
        std::fs::write(left.join("folder/nested/new.txt"), b"new").unwrap();
        std::fs::write(right.join("folder/stale.txt"), b"stale").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "folder");
        open_copy_prompt(&mut app);
        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Right, KeyModifiers::NONE);
        wait_for_copy(&mut app);

        assert_eq!(
            std::fs::read(right.join("folder/nested/new.txt")).unwrap(),
            b"new"
        );
        assert!(!right.join("folder/stale.txt").exists());
    }

    #[test]
    fn diff_copy_rejects_a_source_descendant_changed_after_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir_all(left.join("folder")).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("folder/item.txt"), b"confirmed").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "folder");
        open_copy_prompt(&mut app);
        std::fs::write(left.join("folder/item.txt"), b"changed after confirmation").unwrap();

        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Right, KeyModifiers::NONE);

        assert_eq!(
            app.dialog.as_ref().map(|dialog| dialog.dialog_type),
            Some(DialogType::DiffCopy)
        );
        assert!(app
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.message.contains("tree changed")));
        assert!(app.file_operation_progress.is_none());
        assert!(!right.join("folder").exists());
    }

    #[test]
    fn diff_copy_rejects_a_destination_descendant_changed_after_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir_all(left.join("folder")).unwrap();
        std::fs::create_dir_all(right.join("folder")).unwrap();
        std::fs::write(left.join("folder/item.txt"), b"left version").unwrap();
        std::fs::write(right.join("folder/item.txt"), b"right version").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "folder");
        open_copy_prompt(&mut app);
        std::fs::write(
            right.join("folder/item.txt"),
            b"new destination version after confirmation",
        )
        .unwrap();

        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Right, KeyModifiers::NONE);

        assert_eq!(
            app.dialog.as_ref().map(|dialog| dialog.dialog_type),
            Some(DialogType::DiffCopy)
        );
        assert!(app
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.message.contains("tree changed")));
        assert!(app.file_operation_progress.is_none());
        assert_eq!(
            std::fs::read(right.join("folder/item.txt")).unwrap(),
            b"new destination version after confirmation"
        );
    }

    #[test]
    fn diff_copy_creates_missing_destination_parents() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir_all(left.join("a/b")).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("a/b/item.txt"), b"nested").unwrap();
        std::fs::write(left.join("a/b/next.txt"), b"next").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "a/b/item.txt");
        open_copy_prompt(&mut app);
        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Right, KeyModifiers::NONE);
        wait_for_copy(&mut app);

        assert_eq!(
            app.diff_state
                .as_ref()
                .and_then(|state| state.pending_copy_path.as_deref()),
            Some("a/b/item.txt")
        );
        app.diff_state
            .as_mut()
            .unwrap()
            .finish_copy_operation()
            .unwrap();

        assert_eq!(
            std::fs::read(right.join("a/b/item.txt")).unwrap(),
            b"nested"
        );
        assert_eq!(
            app.diff_state
                .as_ref()
                .unwrap()
                .all_entries
                .iter()
                .find(|entry| entry.relative_path == "a/b/item.txt")
                .map(|entry| entry.status),
            Some(DiffStatus::Same)
        );

        // The first copy created a/b on the right. The remaining row's old
        // absence proof originally stopped at the right root, so targeted
        // reconciliation must rebind that proof without rescanning the row.
        select_entry(&mut app, "a/b/next.txt");
        open_copy_prompt(&mut app);
        assert!(app
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.message.is_empty()));
        cancel_copy_dialog(&mut app);
    }

    #[test]
    fn diff_copy_rejects_a_missing_destination_parent_replaced_after_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir_all(left.join("a")).unwrap();
        std::fs::create_dir_all(right.join("a")).unwrap();
        std::fs::write(left.join("a/item.txt"), b"selected").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "a/item.txt");
        open_copy_prompt(&mut app);
        std::fs::rename(right.join("a"), right.join("a-original")).unwrap();
        std::fs::create_dir(right.join("a")).unwrap();

        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Right, KeyModifiers::NONE);

        assert_eq!(
            app.dialog.as_ref().map(|dialog| dialog.dialog_type),
            Some(DialogType::DiffCopy)
        );
        assert!(app
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.message.contains("replaced")));
        assert!(app.file_operation_progress.is_none());
        assert!(!right.join("a/item.txt").exists());
        assert_eq!(std::fs::read(left.join("a/item.txt")).unwrap(), b"selected");
    }

    #[test]
    fn diff_copy_rejects_a_missing_destination_parent_replaced_since_comparison() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir_all(left.join("a")).unwrap();
        std::fs::create_dir_all(right.join("a")).unwrap();
        std::fs::write(left.join("a/item.txt"), b"selected").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "a/item.txt");
        std::fs::rename(right.join("a"), right.join("a-original")).unwrap();
        std::fs::create_dir(right.join("a")).unwrap();

        open_copy_prompt(&mut app);

        assert!(app
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.message.contains("replaced")));
        assert!(app.file_operation_progress.is_none());
        assert!(!right.join("a/item.txt").exists());
    }

    #[test]
    fn overlapping_diff_roots_disable_copy_and_delete() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("base");
        let right = left.join("folder");
        let source = right.join("folder/source-marker");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"keep me").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "folder");
        open_copy_prompt(&mut app);
        assert!(app
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.message.contains("overlap")));
        assert!(app.file_operation_progress.is_none());

        cancel_copy_dialog(&mut app);
        open_delete_prompt(&mut app);
        assert!(app
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.message.contains("overlap")));
        assert!(app.file_operation_progress.is_none());
        assert_eq!(std::fs::read(&source).unwrap(), b"keep me");
    }

    #[cfg(unix)]
    #[test]
    fn diff_copy_rejects_a_symlinked_source_parent_after_comparison() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(left.join("a")).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(left.join("a/item.txt"), b"selected").unwrap();
        std::fs::write(outside.join("item.txt"), b"outside").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "a/item.txt");
        std::fs::rename(left.join("a"), left.join("a-original")).unwrap();
        std::os::unix::fs::symlink(&outside, left.join("a")).unwrap();

        open_copy_prompt(&mut app);

        assert!(app
            .dialog
            .as_ref()
            .is_some_and(|dialog| !dialog.message.is_empty()));
        assert!(app.file_operation_progress.is_none());
        assert!(!right.join("a/item.txt").exists());
        assert_eq!(std::fs::read(outside.join("item.txt")).unwrap(), b"outside");
    }

    #[test]
    fn unavailable_direction_stays_open_and_up_or_down_cancels() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("left-only.txt"), b"left").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "left-only.txt");
        open_copy_prompt(&mut app);
        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(
            app.dialog.as_ref().map(|dialog| dialog.dialog_type),
            Some(DialogType::DiffCopy)
        );
        assert!(app
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.message.contains("right")));
        assert!(app.file_operation_progress.is_none());

        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert!(app.dialog.is_none());

        open_copy_prompt(&mut app);
        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert!(app.dialog.is_none());
    }

    #[test]
    fn diff_delete_arrows_remove_only_the_requested_side() {
        for (key, delete_left) in [(KeyCode::Left, true), (KeyCode::Right, false)] {
            let temp = tempfile::tempdir().unwrap();
            let left = temp.path().join("left");
            let right = temp.path().join("right");
            std::fs::create_dir(&left).unwrap();
            std::fs::create_dir(&right).unwrap();
            std::fs::write(left.join("item.txt"), b"left").unwrap();
            std::fs::write(right.join("item.txt"), b"right").unwrap();

            let mut app = app_with_diff(&left, &right);
            select_entry(&mut app, "item.txt");
            open_delete_prompt(&mut app);
            crate::ui::dialogs::handle_dialog_input(&mut app, key, KeyModifiers::NONE);

            assert_eq!(
                app.dialog.as_ref().map(|dialog| dialog.dialog_type),
                Some(DialogType::Progress)
            );
            let result = wait_for_delete(&mut app);
            assert_eq!((result.success_count, result.failure_count), (1, 0));
            assert_eq!(left.join("item.txt").exists(), !delete_left);
            assert_eq!(right.join("item.txt").exists(), delete_left);
        }
    }

    #[test]
    fn diff_delete_rejects_a_descendant_changed_after_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir_all(left.join("folder")).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("folder/item.txt"), b"confirmed").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "folder");
        open_delete_prompt(&mut app);
        std::fs::write(left.join("folder/item.txt"), b"changed after confirmation").unwrap();

        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Left, KeyModifiers::NONE);

        assert_eq!(
            app.dialog.as_ref().map(|dialog| dialog.dialog_type),
            Some(DialogType::DiffDelete)
        );
        assert!(app
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.message.contains("tree changed")));
        assert!(app.file_operation_progress.is_none());
        assert_eq!(
            std::fs::read(left.join("folder/item.txt")).unwrap(),
            b"changed after confirmation"
        );
    }

    #[test]
    fn down_deletes_both_sides() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("item.txt"), b"left").unwrap();
        std::fs::write(right.join("item.txt"), b"right").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "item.txt");
        open_delete_prompt(&mut app);
        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Down, KeyModifiers::NONE);

        let result = wait_for_delete(&mut app);
        assert_eq!((result.success_count, result.failure_count), (2, 0));
        assert!(!left.join("item.txt").exists());
        assert!(!right.join("item.txt").exists());
        assert_eq!(
            app.diff_state
                .as_ref()
                .and_then(|state| state.pending_delete_path.as_deref()),
            Some("item.txt")
        );
        app.diff_state
            .as_mut()
            .unwrap()
            .finish_delete_operation()
            .unwrap();
        assert!(app
            .diff_state
            .as_ref()
            .is_some_and(|state| state.all_entries.is_empty()));
    }

    #[test]
    fn down_deletes_both_hard_link_names_without_false_replacement_error() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("item.txt"), b"shared inode").unwrap();
        std::fs::hard_link(left.join("item.txt"), right.join("item.txt")).unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "item.txt");
        open_delete_prompt(&mut app);
        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Down, KeyModifiers::NONE);

        let result = wait_for_delete(&mut app);
        assert_eq!(
            (result.success_count, result.failure_count),
            (2, 0),
            "delete error: {:?}",
            result.last_error
        );
        assert!(!left.join("item.txt").exists());
        assert!(!right.join("item.txt").exists());
    }

    #[test]
    fn down_deletes_the_existing_side_when_the_other_side_is_absent() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("left-only.txt"), b"left").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "left-only.txt");
        open_delete_prompt(&mut app);

        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(
            app.dialog.as_ref().map(|dialog| dialog.dialog_type),
            Some(DialogType::DiffDelete)
        );
        assert!(app
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.message.contains("right")));
        assert!(app.file_operation_progress.is_none());

        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Down, KeyModifiers::NONE);
        let result = wait_for_delete(&mut app);
        assert_eq!((result.success_count, result.failure_count), (1, 0));
        assert!(!left.join("left-only.txt").exists());
    }

    #[test]
    fn delete_both_rejects_a_side_that_appears_after_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("item.txt"), b"confirmed left").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "item.txt");
        open_delete_prompt(&mut app);
        std::fs::write(right.join("item.txt"), b"new right").unwrap();

        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Down, KeyModifiers::NONE);

        assert_eq!(
            app.dialog.as_ref().map(|dialog| dialog.dialog_type),
            Some(DialogType::DiffDelete)
        );
        assert!(app
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.message.contains("appeared since the comparison")));
        assert!(app.file_operation_progress.is_none());
        assert_eq!(
            std::fs::read(left.join("item.txt")).unwrap(),
            b"confirmed left"
        );
        assert_eq!(std::fs::read(right.join("item.txt")).unwrap(), b"new right");
    }

    #[test]
    fn up_cancels_diff_delete_without_changing_either_side() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("item.txt"), b"left").unwrap();
        std::fs::write(right.join("item.txt"), b"right").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "item.txt");
        open_delete_prompt(&mut app);
        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Up, KeyModifiers::NONE);

        assert!(app.dialog.is_none());
        assert!(app.file_operation_progress.is_none());
        assert!(left.join("item.txt").exists());
        assert!(right.join("item.txt").exists());
    }

    #[test]
    fn delete_both_handles_directory_and_file_type_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir_all(left.join("item/nested")).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("item/nested/file.txt"), b"nested").unwrap();
        std::fs::write(right.join("item"), b"regular file").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "item");
        open_delete_prompt(&mut app);
        assert!(app
            .diff_state
            .as_ref()
            .is_some_and(|state| state.delete_prompt_availability() == (true, true, true)));
        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Down, KeyModifiers::NONE);

        let result = wait_for_delete(&mut app);
        assert_eq!((result.success_count, result.failure_count), (2, 0));
        assert!(!left.join("item").exists());
        assert!(!right.join("item").exists());
    }

    #[cfg(unix)]
    #[test]
    fn diff_delete_removes_a_symlink_without_following_it() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        let outside = temp.path().join("outside.txt");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(&outside, b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, left.join("link")).unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "link");
        open_delete_prompt(&mut app);
        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Down, KeyModifiers::NONE);

        let result = wait_for_delete(&mut app);
        assert_eq!((result.success_count, result.failure_count), (1, 0));
        assert!(!left.join("link").exists());
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
    }

    #[test]
    fn diff_delete_rejects_a_target_replaced_after_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("item.txt"), b"confirmed").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "item.txt");
        open_delete_prompt(&mut app);
        std::fs::rename(left.join("item.txt"), left.join("retained.txt")).unwrap();
        std::fs::write(left.join("item.txt"), b"replacement").unwrap();

        crate::ui::dialogs::handle_dialog_input(&mut app, KeyCode::Left, KeyModifiers::NONE);

        assert_eq!(
            app.dialog.as_ref().map(|dialog| dialog.dialog_type),
            Some(DialogType::DiffDelete)
        );
        assert!(app
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.message.contains("changed")));
        assert!(app.file_operation_progress.is_none());
        assert_eq!(
            std::fs::read(left.join("item.txt")).unwrap(),
            b"replacement"
        );
        assert_eq!(
            std::fs::read(left.join("retained.txt")).unwrap(),
            b"confirmed"
        );
    }

    #[test]
    fn diff_delete_rejects_a_file_replaced_by_a_directory_before_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("item"), b"displayed file").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "item");
        std::fs::remove_file(left.join("item")).unwrap();
        std::fs::create_dir(left.join("item")).unwrap();
        std::fs::write(left.join("item/keep.txt"), b"keep").unwrap();

        open_delete_prompt(&mut app);

        assert!(app
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.message.contains("changed since the comparison")));
        assert!(app
            .diff_state
            .as_ref()
            .is_some_and(|state| state.delete_prompt_availability() == (false, false, false)));
        assert!(app.file_operation_progress.is_none());
        assert_eq!(std::fs::read(left.join("item/keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn diff_delete_rejects_a_new_copy_on_a_previously_absent_side() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("item.txt"), b"left").unwrap();

        let mut app = app_with_diff(&left, &right);
        select_entry(&mut app, "item.txt");
        std::fs::write(right.join("item.txt"), b"new right copy").unwrap();

        open_delete_prompt(&mut app);

        assert!(app
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.message.contains("appeared since the comparison")));
        assert!(app.file_operation_progress.is_none());
        assert_eq!(std::fs::read(left.join("item.txt")).unwrap(), b"left");
        assert_eq!(
            std::fs::read(right.join("item.txt")).unwrap(),
            b"new right copy"
        );
    }

    #[test]
    fn two_sided_delete_reports_partial_failure_and_preserves_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("item.txt"), b"left").unwrap();
        std::fs::write(right.join("item.txt"), b"confirmed right").unwrap();

        let left_root = file_ops::capture_directory_authorization(&left).unwrap();
        let right_root = file_ops::capture_directory_authorization(&right).unwrap();
        let left_item = capture_diff_item(&left_root, Path::new("item.txt")).unwrap();
        let right_item = capture_diff_item(&right_root, Path::new("item.txt")).unwrap();
        let targets = vec![
            PreparedDiffDeleteTarget {
                side: "left",
                root: left_root,
                path: left_item.parent.resolved_path().join("item.txt"),
                parent: left_item.parent,
                item: left_item.item,
                tree: left_item.tree,
            },
            PreparedDiffDeleteTarget {
                side: "right",
                root: right_root,
                path: right_item.parent.resolved_path().join("item.txt"),
                parent: right_item.parent,
                item: right_item.item,
                tree: right_item.tree,
            },
        ];
        std::fs::rename(right.join("item.txt"), right.join("retained.txt")).unwrap();
        std::fs::write(right.join("item.txt"), b"replacement").unwrap();

        let (tx, rx) = mpsc::channel();
        run_diff_delete_worker(
            targets,
            Vec::new(),
            PathBuf::from("item.txt"),
            Arc::new(AtomicBool::new(false)),
            tx,
        );
        let mut progress = FileOperationProgress::new(FileOperationType::Delete);
        progress.is_active = true;
        progress.receiver = Some(rx);
        assert!(!progress.poll());
        let result = progress.result.unwrap();

        assert_eq!((result.success_count, result.failure_count), (1, 1));
        assert!(result
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("right") && error.contains("changed")));
        assert!(!left.join("item.txt").exists());
        assert_eq!(
            std::fs::read(right.join("item.txt")).unwrap(),
            b"replacement"
        );
        assert_eq!(
            std::fs::read(right.join("retained.txt")).unwrap(),
            b"confirmed right"
        );
    }

    #[test]
    fn delete_worker_rechecks_an_absent_side_before_deleting_the_other_side() {
        let temp = tempfile::tempdir().unwrap();
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        std::fs::write(left.join("item.txt"), b"confirmed left").unwrap();

        let left_root = file_ops::capture_directory_authorization(&left).unwrap();
        let right_root = file_ops::capture_directory_authorization(&right).unwrap();
        let left_item = capture_diff_item(&left_root, Path::new("item.txt")).unwrap();
        let right_missing = file_ops::capture_missing_path_authorization(
            &right_root,
            Path::new("item.txt"),
            "Diff right delete target",
        )
        .unwrap();
        let targets = vec![PreparedDiffDeleteTarget {
            side: "left",
            root: left_root,
            path: left_item.parent.resolved_path().join("item.txt"),
            parent: left_item.parent,
            item: left_item.item,
            tree: left_item.tree,
        }];

        std::fs::write(right.join("item.txt"), b"late right").unwrap();

        let (tx, rx) = mpsc::channel();
        run_diff_delete_worker(
            targets,
            vec![("right", right_missing)],
            PathBuf::from("item.txt"),
            Arc::new(AtomicBool::new(false)),
            tx,
        );
        let mut progress = FileOperationProgress::new(FileOperationType::Delete);
        progress.is_active = true;
        progress.receiver = Some(rx);
        assert!(!progress.poll());
        let result = progress.result.unwrap();

        assert_eq!((result.success_count, result.failure_count), (0, 1));
        assert!(result
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("right") && error.contains("appeared")));
        assert_eq!(
            std::fs::read(left.join("item.txt")).unwrap(),
            b"confirmed left"
        );
        assert_eq!(
            std::fs::read(right.join("item.txt")).unwrap(),
            b"late right"
        );
    }
}
