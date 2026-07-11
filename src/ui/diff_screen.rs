use std::collections::HashSet;
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

use super::app::{App, Screen, SortBy, SortOrder};
use super::theme::Theme;
use crate::utils::format::{format_size, safe_suffix};

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
        self.cancel_flag = Arc::new(AtomicBool::new(false));

        let (result_tx, result_rx) = mpsc::channel();
        let (progress_tx, progress_rx) = mpsc::channel();
        self.receiver = Some(result_rx);
        self.progress_receiver = Some(progress_rx);

        let left_root = self.left_root.clone();
        let right_root = self.right_root.clone();
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
    }

    /// Build the flat diff list by recursively comparing both directory trees (synchronous)
    pub fn build_diff_list(&mut self) {
        self.all_entries.clear();
        let left_root = self.left_root.clone();
        let right_root = self.right_root.clone();
        build_recursive(
            &left_root,
            &right_root,
            "",
            0,
            self.compare_method,
            self.sort_by,
            self.sort_order,
            &mut self.all_entries,
        );
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

    /// Collapse the current directory by one level (Left arrow)
    /// Only collapses if expanded; descendant directories also get collapsed
    pub fn collapse_one_level(&mut self) {
        if let Some(entry) = self.current_entry() {
            if entry.is_directory && !self.collapsed_dirs.contains(&entry.relative_path) {
                let path = entry.relative_path.clone();
                let current_all_idx = self.filtered_indices.get(self.selected_index).copied();
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
        let entry = &self.all_entries[all_entry_idx];
        if !entry.children_not_loaded || !entry.is_directory {
            return;
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
        let names = read_dir_names(&dir_path);

        let mut sorted_names = names;
        sort_names_one_side(&mut sorted_names, &dir_path);

        let mut children = Vec::new();
        for name in &sorted_names {
            let child_relative = format!("{}/{}", relative_path, name);
            let full_path = dir_path.join(name);
            let info = make_file_info(&full_path, name);
            let is_dir = info.as_ref().map_or(false, |i| i.is_directory);
            let status = if is_left {
                DiffStatus::LeftOnly
            } else {
                DiffStatus::RightOnly
            };

            let (left, right) = if is_left { (info, None) } else { (None, info) };

            children.push(DiffEntry {
                relative_path: child_relative,
                left,
                right,
                status,
                is_directory: is_dir,
                depth: parent_depth + 1,
                children_not_loaded: is_dir,
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
) -> BuildFrame {
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
    let left_names_vec = read_dir_names(&left_dir);
    let right_names_vec = read_dir_names(&right_dir);
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

    BuildFrame {
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
    }
}

fn build_iterative(
    left_root: &Path,
    right_root: &Path,
    compare_method: CompareMethod,
    sort_by: SortBy,
    sort_order: SortOrder,
    entries: &mut Vec<DiffEntry>,
    progress: Option<BuildProgress<'_>>,
) {
    let mut frames = vec![make_build_frame(
        left_root,
        right_root,
        String::new(),
        0,
        None,
        sort_by,
        sort_order,
    )];

    while !frames.is_empty() {
        if progress
            .as_ref()
            .is_some_and(|p| p.cancel_flag.load(Ordering::Relaxed))
        {
            return;
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
                });
                frames.push(make_build_frame(
                    left_root,
                    right_root,
                    relative_path,
                    depth + 1,
                    Some(dir_index),
                    sort_by,
                    sort_order,
                ));
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
            });
            frames
                .last_mut()
                .expect("current directory frame")
                .has_difference = true;
        }
    }
}

fn build_recursive(
    left_root: &Path,
    right_root: &Path,
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
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Build DiffFileInfo from a path, returning None if the path doesn't exist
fn make_file_info(path: &Path, name: &str) -> Option<DiffFileInfo> {
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

    Some(DiffFileInfo {
        name: name.to_string(),
        size,
        modified,
        is_directory,
        is_symlink,
        full_path: path.to_path_buf(),
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
    draw_function_bar(frame, fn_bar_area, theme, kb);
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
            }),
            right: None,
            status: DiffStatus::LeftOnly,
            is_directory: true,
            depth,
            children_not_loaded: false,
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
}
