use std::path::{Path, PathBuf};
use std::process::Command;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use super::app::{App, Screen};
use super::theme::Theme;
use crate::utils::format::{pad_to_display_width, truncate_to_display_width};

// ═══════════════════════════════════════════════════════════════════════════════
// 데이터 구조
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitTab {
    Commit,
    Log,
    Branch,
}

#[derive(Debug, Clone)]
pub struct GitFileEntry {
    pub path: String,
    pub index_status: char,
    pub worktree_status: char,
    pub staged: bool,
}

#[derive(Debug, Clone)]
pub struct GitLogEntry {
    pub hash: String,
    pub message: String,
    pub author: String,
    pub date: String,
    pub refs: String,
}

#[derive(Debug, Clone)]
pub struct GitBranchEntry {
    pub name: String,
    pub is_current: bool,
    pub is_remote: bool,
}

#[derive(Debug, Clone)]
pub enum InputMode {
    BranchCreate,
    CommitAmend,
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    BranchDelete(String),
    RestoreToCommit(String), // hash
}

pub struct GitScreenState {
    pub repo_path: PathBuf,
    pub current_tab: GitTab,
    pub branch_name: String,

    // Status tab
    pub status_files: Vec<GitFileEntry>,
    pub status_selected: usize,
    pub status_scroll: usize,

    // Commit tab
    pub commit_message: String,
    pub commit_input_active: bool,
    pub commit_selected: usize,
    pub commit_scroll: usize,

    // Log tab
    pub log_entries: Vec<GitLogEntry>,
    pub log_selected: usize,
    pub log_scroll: usize,
    pub log_detail: Option<String>,
    pub log_detail_scroll: usize,

    // Branch tab
    pub branches: Vec<GitBranchEntry>,
    pub branch_selected: usize,
    pub branch_scroll: usize,

    // Dialog
    pub input_mode: Option<InputMode>,
    pub input_buffer: String,
    pub confirm_action: Option<ConfirmAction>,
    pub confirm_selected_button: usize, // 0: Yes, 1: No
    pub message: Option<String>,
    pub message_timer: u8,
}

impl GitScreenState {
    pub fn new(repo_path: PathBuf) -> Self {
        stage_all(&repo_path);
        let branch_name = get_current_branch(&repo_path);
        let status_files = get_status(&repo_path);
        let log_entries = get_log(&repo_path, 200);
        let branches = get_branches(&repo_path);

        Self {
            repo_path,
            current_tab: GitTab::Commit,
            branch_name,
            status_files,
            status_selected: 0,
            status_scroll: 0,
            commit_message: String::new(),
            commit_input_active: false,
            commit_selected: 0,
            commit_scroll: 0,
            log_entries,
            log_selected: 0,
            log_scroll: 0,
            log_detail: None,
            log_detail_scroll: 0,
            branches,
            branch_selected: 0,
            branch_scroll: 0,
            input_mode: None,
            input_buffer: String::new(),
            confirm_action: None,
            confirm_selected_button: 1, // Default: No
            message: None,
            message_timer: 0,
        }
    }

    fn refresh_status(&mut self) {
        self.branch_name = get_current_branch(&self.repo_path);
        self.status_files = get_status(&self.repo_path);
        let len = self.status_files.len();
        if self.status_selected >= len {
            self.status_selected = len.saturating_sub(1);
        }
        if self.commit_selected >= len {
            self.commit_selected = len.saturating_sub(1);
        }
    }

    fn refresh_all(&mut self) {
        self.refresh_status();
        self.log_entries = get_log(&self.repo_path, 200);
        self.branches = get_branches(&self.repo_path);
        if self.log_selected >= self.log_entries.len() {
            self.log_selected = self.log_entries.len().saturating_sub(1);
        }
        if self.branch_selected >= self.branches.len() {
            self.branch_selected = self.branches.len().saturating_sub(1);
        }
    }

    fn show_msg(&mut self, msg: &str) {
        self.message = Some(msg.to_string());
        self.message_timer = 4;
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Git CLI 함수들
// ═══════════════════════════════════════════════════════════════════════════════

/// Create a git Command with safe.directory configured for the given path
fn git_cmd(path: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-c")
        .arg(format!("safe.directory={}", path.to_string_lossy()));
    cmd.arg("-C").arg(path);
    cmd
}

/// Create a git Command with user identity fallback for commit operations
fn git_commit_cmd(path: &Path) -> Command {
    let mut cmd = git_cmd(path);
    // Check if user.name is configured
    let has_name = git_cmd(path)
        .args(["config", "user.name"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let has_email = git_cmd(path)
        .args(["config", "user.email"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !has_name {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "User".to_string());
        cmd.arg("-c").arg(format!("user.name={}", user));
    }
    if !has_email {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "user".to_string());
        cmd.arg("-c").arg(format!("user.email={}@localhost", user));
    }
    cmd
}

pub fn is_git_repo(path: &Path) -> bool {
    git_cmd(path)
        .args(["rev-parse", "--git-dir"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Public wrapper for get_log()
pub fn get_log_public(path: &Path, count: usize) -> Vec<GitLogEntry> {
    get_log(path, count)
}

/// Get the repo root directory using git rev-parse --show-toplevel
pub fn get_repo_root(path: &Path) -> Option<PathBuf> {
    git_cmd(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string()))
}

/// Public wrapper for git_cmd() - for external checkout operations
pub fn git_cmd_public(path: &Path) -> Command {
    git_cmd(path)
}

fn get_current_branch(path: &Path) -> String {
    git_cmd(path)
        .args(["branch", "--show-current"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() {
                    // detached HEAD
                    git_cmd(path)
                        .args(["rev-parse", "--short", "HEAD"])
                        .output()
                        .ok()
                        .map(|o2| format!("({})", String::from_utf8_lossy(&o2.stdout).trim()))
                } else {
                    Some(s)
                }
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn get_status(path: &Path) -> Vec<GitFileEntry> {
    let output = git_cmd(path).args(["status", "--porcelain=v1"]).output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();

    for line in stdout.lines() {
        if line.len() < 3 {
            continue;
        }
        let bytes = line.as_bytes();
        let index_status = bytes[0] as char;
        let worktree_status = bytes[1] as char;
        let file_path = &line[3..];

        // Handle rename: "R  old -> new"
        let display_path = if let Some(pos) = file_path.find(" -> ") {
            file_path[pos + 4..].to_string()
        } else {
            file_path.to_string()
        };

        let staged = index_status != ' ' && index_status != '?';

        entries.push(GitFileEntry {
            path: display_path,
            index_status,
            worktree_status,
            staged,
        });
    }

    entries
}

fn get_log(path: &Path, count: usize) -> Vec<GitLogEntry> {
    let count_str = count.to_string();
    let output = git_cmd(path)
        .args(["log", "--format=%h|%s|%an|%ar|%D", "-n", &count_str])
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(5, '|').collect();
        if parts.len() >= 4 {
            entries.push(GitLogEntry {
                hash: parts[0].to_string(),
                message: parts.get(1).unwrap_or(&"").to_string(),
                author: parts.get(2).unwrap_or(&"").to_string(),
                date: parts.get(3).unwrap_or(&"").to_string(),
                refs: parts.get(4).unwrap_or(&"").to_string(),
            });
        }
    }

    entries
}

fn get_commit_diff(path: &Path, hash: &str) -> String {
    // Validate hash to prevent command injection
    if !hash.chars().all(|c| c.is_ascii_alphanumeric()) {
        return String::new();
    }
    git_cmd(path)
        .args(["show", "--stat", "--patch", hash])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

fn get_file_diff(path: &Path, file: &str, staged: bool) -> String {
    let mut cmd = git_cmd(path);
    cmd.arg("diff");
    if staged {
        cmd.arg("--cached");
    }
    cmd.args(["--", file]);

    cmd.output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

fn get_branches(path: &Path) -> Vec<GitBranchEntry> {
    let output = git_cmd(path)
        .args([
            "branch",
            "--format=%(HEAD)|%(refname:short)|%(objectname:short)|%(refname)",
        ])
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(4, '|').collect();
        if parts.len() >= 2 {
            let is_current = parts[0] == "*";
            let name = parts[1].to_string();
            let is_remote = parts.get(3).is_some_and(|r| r.starts_with("refs/remotes/"));

            entries.push(GitBranchEntry {
                name,
                is_current,
                is_remote,
            });
        }
    }

    entries
}

fn stage_all(path: &Path) {
    let _ = git_cmd(path).args(["add", "-A"]).output();
}

fn stage_file(path: &Path, file: &str) -> Result<(), String> {
    let output = git_cmd(path)
        .args(["add", "--", file])
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

fn unstage_file(path: &Path, file: &str) -> Result<(), String> {
    let output = git_cmd(path)
        .args(["reset", "HEAD", "--", file])
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

fn do_commit(path: &Path, message: &str) -> Result<String, String> {
    let output = git_commit_cmd(path)
        .args(["commit", "-m", message])
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn do_commit_amend(path: &Path, message: &str) -> Result<String, String> {
    let output = git_commit_cmd(path)
        .args(["commit", "--amend", "-m", message])
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn checkout_branch(path: &Path, branch: &str) -> Result<(), String> {
    // Validate branch name
    if branch.contains("..") || branch.contains("~") || branch.starts_with('-') {
        return Err("Invalid branch name".to_string());
    }
    let output = git_cmd(path)
        .args(["checkout", branch])
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn create_branch(path: &Path, name: &str) -> Result<(), String> {
    if name.contains("..") || name.contains("~") || name.starts_with('-') || name.contains(' ') {
        return Err("Invalid branch name".to_string());
    }
    let output = git_cmd(path)
        .args(["checkout", "-b", name])
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn force_delete_branch(path: &Path, name: &str) -> Result<(), String> {
    if name.contains("..") || name.starts_with('-') {
        return Err("Invalid branch name".to_string());
    }
    let output = git_cmd(path)
        .args(["branch", "-D", name])
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn restore_to_commit(path: &Path, hash: &str) -> Result<String, String> {
    // Validate hash to prevent command injection
    if !hash.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("Invalid commit hash".to_string());
    }

    // Reset index and working directory to the target commit's tree
    let output = git_cmd(path)
        .args(["read-tree", "--reset", "-u", hash])
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(format!("Restored to {}", hash))
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Find which commit hash the current index tree matches.
/// Always returns a commit hash — falls back to HEAD if no exact match.
fn get_index_matching_commit(path: &Path) -> Option<String> {
    // Get current index tree hash
    let index_tree = git_cmd(path)
        .args(["write-tree"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;

    // Search commits for matching tree
    let output = git_cmd(path)
        .args(["log", "--format=%h %T", "-n", "200"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let mut parts = line.splitn(2, ' ');
        let hash = parts.next()?;
        let tree = parts.next()?;
        if tree == index_tree {
            return Some(hash.to_string());
        }
    }

    None
}

// ═══════════════════════════════════════════════════════════════════════════════
// 그리기 함수
// ═══════════════════════════════════════════════════════════════════════════════

pub fn draw(frame: &mut Frame, state: &mut GitScreenState, area: Rect, theme: &Theme) {
    let colors = &theme.git_screen;

    // Fill background
    let bg = Block::default().style(Style::default().bg(colors.bg));
    frame.render_widget(bg, area);

    // Layout: Header(1) + TabBar(1) + Content(fill) + Footer(1)
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(1), // tab bar
            Constraint::Min(3),    // content
            Constraint::Length(1), // footer
        ])
        .split(area);

    draw_header(frame, state, layout[0], colors);
    draw_tab_bar(frame, state, layout[1], colors);
    draw_content(frame, state, layout[2], colors);
    draw_footer(frame, state, layout[3], colors);

    // Draw input dialog overlay
    if state.input_mode.is_some() {
        draw_input_dialog(frame, state, area, colors);
    }

    // Draw confirm dialog overlay
    if state.confirm_action.is_some() {
        draw_confirm_dialog(frame, state, area, theme);
    }

    // Update message timer
    if state.message_timer > 0 {
        state.message_timer -= 1;
        if state.message_timer == 0 {
            state.message = None;
        }
    }
}

fn draw_header(
    frame: &mut Frame,
    state: &GitScreenState,
    area: Rect,
    colors: &super::theme::GitScreenColors,
) {
    let max_w = area.width as usize;
    let path_str = state.repo_path.to_string_lossy();

    // " [branch] path" — truncate path to fit remaining width
    let prefix = format!(" [{}] ", state.branch_name);
    let prefix_w = UnicodeWidthStr::width(prefix.as_str());
    let path_max = max_w.saturating_sub(prefix_w);
    let truncated_path = truncate_to_display_width(&path_str, path_max);

    let spans = vec![
        Span::styled(" [", Style::default().fg(colors.header_path)),
        Span::styled(
            &state.branch_name,
            Style::default()
                .fg(colors.header_branch)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("] ", Style::default().fg(colors.header_path)),
        Span::styled(truncated_path, Style::default().fg(colors.header_path)),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_tab_bar(
    frame: &mut Frame,
    state: &GitScreenState,
    area: Rect,
    colors: &super::theme::GitScreenColors,
) {
    let tabs = [
        (GitTab::Commit, "1:Commit"),
        (GitTab::Log, "2:Log"),
        (GitTab::Branch, "3:Branch"),
    ];

    let bg_style = Style::default().bg(colors.tab_bar_bg);
    let bg_block = Block::default().style(bg_style);
    frame.render_widget(bg_block, area);

    let mut spans = vec![Span::styled(" ", bg_style)];
    for (tab, label) in &tabs {
        let style = if *tab == state.current_tab {
            Style::default()
                .fg(colors.tab_active)
                .bg(colors.tab_bar_bg)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default()
                .fg(colors.tab_inactive)
                .bg(colors.tab_bar_bg)
        };
        spans.push(Span::styled(format!(" {} ", label), style));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_content(
    frame: &mut Frame,
    state: &mut GitScreenState,
    area: Rect,
    colors: &super::theme::GitScreenColors,
) {
    match state.current_tab {
        GitTab::Commit => draw_commit_tab(frame, state, area, colors),
        GitTab::Log => draw_log_tab(frame, state, area, colors),
        GitTab::Branch => draw_branch_tab(frame, state, area, colors),
    }
}

fn file_status_style(entry: &GitFileEntry, colors: &super::theme::GitScreenColors) -> Style {
    if entry.staged {
        Style::default().fg(colors.file_staged)
    } else if entry.index_status == '?' {
        Style::default().fg(colors.file_untracked)
    } else if entry.worktree_status == 'D' || entry.index_status == 'D' {
        Style::default().fg(colors.file_deleted)
    } else {
        Style::default().fg(colors.file_modified)
    }
}

fn file_status_char(entry: &GitFileEntry) -> &str {
    if entry.staged {
        match entry.index_status {
            'A' => "A",
            'D' => "D",
            'R' => "R",
            'M' => "M",
            _ => "M",
        }
    } else if entry.index_status == '?' {
        "?"
    } else if entry.worktree_status == 'D' {
        "D"
    } else {
        "M"
    }
}

fn draw_commit_tab(
    frame: &mut Frame,
    state: &mut GitScreenState,
    area: Rect,
    colors: &super::theme::GitScreenColors,
) {
    // If showing a diff detail, render it over the full area
    if state.log_detail.is_some() {
        draw_diff_detail(frame, state, area, colors, false);
        return;
    }

    // Split: file list (top) + message input (bottom 3 lines)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // file list
            Constraint::Length(3), // commit message input
        ])
        .split(area);

    // Draw file list (same as status but with stage/unstage indicators)
    let file_area = chunks[0];
    let visible_height = file_area.height as usize;

    if !state.commit_input_active {
        // File list mode
        if state.commit_selected < state.commit_scroll {
            state.commit_scroll = state.commit_selected;
        }
        if state.commit_selected >= state.commit_scroll + visible_height {
            state.commit_scroll = state.commit_selected - visible_height + 1;
        }
    }

    if state.status_files.is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            "  No changes to commit",
            Style::default().fg(colors.footer_text),
        )));
        frame.render_widget(msg, file_area);
    } else {
        let mut lines = Vec::new();
        for (i, entry) in state
            .status_files
            .iter()
            .enumerate()
            .skip(state.commit_scroll)
            .take(visible_height)
        {
            let is_selected = !state.commit_input_active && i == state.commit_selected;
            let status_char = file_status_char(entry);
            let prefix = if entry.staged { "+" } else { " " };
            let text = format!(" {}[{}] {}", prefix, status_char, entry.path);

            let style = if is_selected {
                Style::default()
                    .fg(colors.selected_text)
                    .bg(colors.selected_bg)
            } else {
                file_status_style(entry, colors)
            };

            let line_text = pad_to_display_width(&text, file_area.width as usize);

            lines.push(Line::from(Span::styled(line_text, style)));
        }
        frame.render_widget(Paragraph::new(lines), file_area);

        // Scrollbar
        if state.status_files.len() > visible_height {
            let mut scrollbar_state =
                ScrollbarState::new(state.status_files.len()).position(state.commit_scroll);
            let scrollbar_area = Rect::new(
                file_area.x + file_area.width.saturating_sub(1),
                file_area.y,
                1,
                file_area.height,
            );
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                scrollbar_area,
                &mut scrollbar_state,
            );
        }
    }

    // Draw commit message input area
    let input_area = chunks[1];
    let border_style = if state.commit_input_active {
        Style::default().fg(colors.tab_active)
    } else {
        Style::default().fg(colors.commit_input_border)
    };

    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(" Commit Message ");

    let inner_width = input_area.width.saturating_sub(2) as usize; // border 2칸 제외
    let display_text = if state.commit_message.is_empty() && !state.commit_input_active {
        "Press Tab to enter commit message...".to_string()
    } else {
        truncate_to_display_width(&state.commit_message, inner_width)
    };

    let text_style = if state.commit_message.is_empty() && !state.commit_input_active {
        Style::default().fg(colors.footer_text)
    } else {
        Style::default().fg(colors.commit_input_text)
    };

    let input_paragraph = Paragraph::new(Span::styled(display_text, text_style)).block(input_block);
    frame.render_widget(input_paragraph, input_area);

    // Show cursor when input is active
    if state.commit_input_active {
        let cursor_x =
            input_area.x + 1 + UnicodeWidthStr::width(state.commit_message.as_str()) as u16;
        let cursor_y = input_area.y + 1;
        if cursor_x < input_area.x + input_area.width.saturating_sub(1) {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }
}

fn draw_log_tab(
    frame: &mut Frame,
    state: &mut GitScreenState,
    area: Rect,
    colors: &super::theme::GitScreenColors,
) {
    if state.log_entries.is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            "  No commits",
            Style::default().fg(colors.footer_text),
        )));
        frame.render_widget(msg, area);
        return;
    }

    // If detail view is open, split view
    if state.log_detail.is_some() {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(area);

        draw_log_list(frame, state, chunks[0], colors);
        draw_diff_detail(frame, state, chunks[1], colors, true);
    } else {
        draw_log_list(frame, state, area, colors);
    }
}

fn draw_log_list(
    frame: &mut Frame,
    state: &mut GitScreenState,
    area: Rect,
    colors: &super::theme::GitScreenColors,
) {
    let visible_height = area.height as usize;

    if state.log_selected < state.log_scroll {
        state.log_scroll = state.log_selected;
    }
    if state.log_selected >= state.log_scroll + visible_height {
        state.log_scroll = state.log_selected - visible_height + 1;
    }

    let mut lines = Vec::new();
    let max_width = area.width as usize;

    // Detect if files have been restored to a different commit
    let restored = get_index_matching_commit(&state.repo_path);

    for (i, entry) in state
        .log_entries
        .iter()
        .enumerate()
        .skip(state.log_scroll)
        .take(visible_height)
    {
        let is_selected = i == state.log_selected;
        let is_restored = restored.as_deref() == Some(&entry.hash);
        let marker = if is_restored { ">" } else { " " };

        if is_selected {
            let text = format!(
                "{}{} {} ({}, {})",
                marker, entry.hash, entry.message, entry.author, entry.date
            );
            let display = pad_to_display_width(&text, max_width);
            lines.push(Line::from(Span::styled(
                display,
                Style::default()
                    .fg(colors.selected_text)
                    .bg(colors.selected_bg),
            )));
        } else {
            let marker_style = if is_restored {
                Style::default().fg(colors.file_staged)
            } else {
                Style::default()
            };
            let mut spans = vec![
                Span::styled(marker, marker_style),
                Span::styled(&entry.hash, Style::default().fg(colors.log_hash)),
                Span::styled(" ", Style::default()),
                Span::styled(&entry.message, Style::default().fg(colors.log_message)),
                Span::styled(" (", Style::default().fg(colors.log_date)),
                Span::styled(&entry.author, Style::default().fg(colors.log_author)),
                Span::styled(", ", Style::default().fg(colors.log_date)),
                Span::styled(&entry.date, Style::default().fg(colors.log_date)),
                Span::styled(")", Style::default().fg(colors.log_date)),
            ];

            lines.push(Line::from(spans));
        }
    }

    frame.render_widget(Paragraph::new(lines), area);

    // Scrollbar
    if state.log_entries.len() > visible_height {
        let mut scrollbar_state =
            ScrollbarState::new(state.log_entries.len()).position(state.log_scroll);
        let scrollbar_area = Rect::new(
            area.x + area.width.saturating_sub(1),
            area.y,
            1,
            area.height,
        );
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            scrollbar_area,
            &mut scrollbar_state,
        );
    }
}

fn draw_diff_detail(
    frame: &mut Frame,
    state: &mut GitScreenState,
    area: Rect,
    colors: &super::theme::GitScreenColors,
    with_left_border: bool,
) {
    let detail = match &state.log_detail {
        Some(d) => d,
        None => return,
    };

    let diff_lines: Vec<&str> = detail.lines().collect();
    let visible_height = area.height as usize;

    let max_scroll = diff_lines.len().saturating_sub(visible_height);
    if state.log_detail_scroll > max_scroll {
        state.log_detail_scroll = max_scroll;
    }

    let mut lines = Vec::new();
    let max_width = area.width as usize;

    for line in diff_lines
        .iter()
        .skip(state.log_detail_scroll)
        .take(visible_height)
    {
        let truncated = truncate_to_display_width(line, max_width);

        let style = if line.starts_with('+') && !line.starts_with("+++") {
            Style::default().fg(colors.diff_add)
        } else if line.starts_with('-') && !line.starts_with("---") {
            Style::default().fg(colors.diff_remove)
        } else if line.starts_with("@@") || line.starts_with("diff ") || line.starts_with("index ")
        {
            Style::default()
                .fg(colors.diff_header)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors.log_message)
        };

        lines.push(Line::from(Span::styled(truncated, style)));
    }

    if with_left_border {
        let block = Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(colors.border));
        frame.render_widget(Paragraph::new(lines).block(block), area);
    } else {
        frame.render_widget(Paragraph::new(lines), area);
    }

    // Scrollbar
    if diff_lines.len() > visible_height {
        let mut scrollbar_state =
            ScrollbarState::new(diff_lines.len()).position(state.log_detail_scroll);
        let scrollbar_area = Rect::new(
            area.x + area.width.saturating_sub(1),
            area.y,
            1,
            area.height,
        );
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            scrollbar_area,
            &mut scrollbar_state,
        );
    }
}

fn draw_branch_tab(
    frame: &mut Frame,
    state: &mut GitScreenState,
    area: Rect,
    colors: &super::theme::GitScreenColors,
) {
    if state.branches.is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            "  No branches",
            Style::default().fg(colors.footer_text),
        )));
        frame.render_widget(msg, area);
        return;
    }

    let visible_height = area.height as usize;

    if state.branch_selected < state.branch_scroll {
        state.branch_scroll = state.branch_selected;
    }
    if state.branch_selected >= state.branch_scroll + visible_height {
        state.branch_scroll = state.branch_selected - visible_height + 1;
    }

    let mut lines = Vec::new();
    let max_width = area.width as usize;

    for (i, branch) in state
        .branches
        .iter()
        .enumerate()
        .skip(state.branch_scroll)
        .take(visible_height)
    {
        let is_selected = i == state.branch_selected;
        let prefix = if branch.is_current { "* " } else { "  " };
        let text = format!(" {}{}", prefix, branch.name);

        let style = if is_selected {
            Style::default()
                .fg(colors.selected_text)
                .bg(colors.selected_bg)
        } else if branch.is_current {
            Style::default()
                .fg(colors.branch_current)
                .add_modifier(Modifier::BOLD)
        } else if branch.is_remote {
            Style::default()
                .fg(colors.footer_text)
                .add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(colors.branch_normal)
        };

        let display = pad_to_display_width(&text, max_width);

        lines.push(Line::from(Span::styled(display, style)));
    }

    frame.render_widget(Paragraph::new(lines), area);

    // Scrollbar
    if state.branches.len() > visible_height {
        let mut scrollbar_state =
            ScrollbarState::new(state.branches.len()).position(state.branch_scroll);
        let scrollbar_area = Rect::new(
            area.x + area.width.saturating_sub(1),
            area.y,
            1,
            area.height,
        );
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            scrollbar_area,
            &mut scrollbar_state,
        );
    }
}

fn draw_footer(
    frame: &mut Frame,
    state: &GitScreenState,
    area: Rect,
    colors: &super::theme::GitScreenColors,
) {
    // Show message if present
    if let Some(ref msg) = state.message {
        let raw = format!(" {} ", msg);
        let display = truncate_to_display_width(&raw, area.width as usize);
        let message = Paragraph::new(Span::styled(
            display,
            Style::default()
                .fg(colors.footer_text)
                .add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(message, area);
        return;
    }

    let shortcuts: Vec<(&str, &str)> = match state.current_tab {
        GitTab::Commit => {
            if state.commit_input_active {
                vec![("Enter", "commit "), ("Tab", "files "), ("Esc", "cancel")]
            } else if state.log_detail.is_some() {
                vec![
                    ("\u{2191}\u{2193}", "nav "),
                    ("PgUp/Dn", "scroll "),
                    ("Esc", "close"),
                ]
            } else {
                vec![
                    ("\u{2191}\u{2193}", "nav "),
                    ("Spc", "stage "),
                    ("Enter", "diff "),
                    ("^a", "ll "),
                    ("Tab", "msg "),
                    ("a", "mend "),
                    ("\u{2190}\u{2192}", "tab "),
                    ("Esc", "back"),
                ]
            }
        }
        GitTab::Log => {
            if state.log_detail.is_some() {
                vec![
                    ("\u{2191}\u{2193}", "nav "),
                    ("\u{2190}\u{2192}", "commit "),
                    ("PgUp/Dn", "scroll "),
                    ("Esc", "close"),
                ]
            } else {
                vec![
                    ("\u{2191}\u{2193}", "nav "),
                    ("Enter", "detail "),
                    ("r", "estore "),
                    ("\u{2190}\u{2192}", "tab "),
                    ("Esc", "back"),
                ]
            }
        }
        GitTab::Branch => vec![
            ("\u{2191}\u{2193}", "nav "),
            ("Enter", "checkout "),
            ("c", "heckout "),
            ("n", "ew "),
            ("x", "del "),
            ("\u{2190}\u{2192}", "tab "),
            ("Esc", "back"),
        ],
    };

    let mut spans = Vec::new();
    for (key, rest) in &shortcuts {
        spans.push(Span::styled(*key, Style::default().fg(colors.footer_key)));
        spans.push(Span::styled(":", Style::default().fg(colors.footer_text)));
        spans.push(Span::styled(*rest, Style::default().fg(colors.footer_text)));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_input_dialog(
    frame: &mut Frame,
    state: &GitScreenState,
    area: Rect,
    colors: &super::theme::GitScreenColors,
) {
    let title = match &state.input_mode {
        Some(InputMode::BranchCreate) => " New Branch Name ",
        Some(InputMode::CommitAmend) => " Amend Message ",
        None => return,
    };

    let width = 50u16.min(area.width.saturating_sub(4));
    // Clamp to the terminal area, or Clear renders out of bounds and panics on
    // terminals shorter than 3 rows.
    let height = 3u16.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(colors.tab_active))
        .title(title)
        .style(Style::default().bg(colors.bg));

    let dialog_inner_width = width.saturating_sub(2) as usize; // border 2칸 제외
    let display_buf = truncate_to_display_width(&state.input_buffer, dialog_inner_width);
    let input = Paragraph::new(Span::styled(
        display_buf,
        Style::default().fg(colors.commit_input_text),
    ))
    .block(block);

    frame.render_widget(Clear, dialog_area);
    frame.render_widget(input, dialog_area);

    // Cursor
    let cursor_x = dialog_area.x + 1 + UnicodeWidthStr::width(state.input_buffer.as_str()) as u16;
    let cursor_y = dialog_area.y + 1;
    if cursor_x < dialog_area.x + dialog_area.width.saturating_sub(1) {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn draw_confirm_dialog(
    frame: &mut Frame,
    state: &GitScreenState,
    area: Rect,
    theme: &super::theme::Theme,
) {
    let (msg, title) = match &state.confirm_action {
        Some(ConfirmAction::BranchDelete(name)) => {
            (format!("Delete branch '{}'?", name), " Delete ")
        }
        Some(ConfirmAction::RestoreToCommit(hash)) => {
            (format!("Restore files to {}?", hash), " Restore ")
        }
        None => return,
    };
    let cd = &theme.confirm_dialog;

    let width = 50u16.min(area.width.saturating_sub(4));
    // Clamp height to the area too, or Clear/Paragraph render out of bounds and panic
    // on terminals shorter than 6 rows.
    let height = 6u16.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(cd.border))
        .title(title)
        .title_style(Style::default().fg(cd.title).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(cd.bg));

    let inner = block.inner(dialog_area);
    frame.render_widget(Clear, dialog_area);
    frame.render_widget(block, dialog_area);

    // Message — intersect with inner: on very short terminals the fixed row offsets fall
    // outside the clamped dialog, and Paragraph indexes the buffer directly (panics OOB).
    let message_area =
        Rect::new(inner.x + 1, inner.y + 1, inner.width.saturating_sub(2), 1).intersection(inner);
    frame.render_widget(
        Paragraph::new(msg)
            .style(Style::default().fg(cd.message_text))
            .alignment(ratatui::layout::Alignment::Center),
        message_area,
    );

    // Buttons
    let selected_style = Style::default()
        .fg(cd.button_selected_text)
        .bg(cd.button_selected_bg);
    let normal_style = Style::default().fg(cd.button_text);

    let yes_style = if state.confirm_selected_button == 0 {
        selected_style
    } else {
        normal_style
    };
    let no_style = if state.confirm_selected_button == 1 {
        selected_style
    } else {
        normal_style
    };

    let buttons = Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(" Yes ", yes_style),
        Span::styled("    ", Style::default()),
        Span::styled(" No ", no_style),
        Span::styled("  ", Style::default()),
    ]);
    let button_area = Rect::new(
        inner.x + 1,
        inner.y + inner.height.saturating_sub(2),
        inner.width.saturating_sub(2),
        1,
    )
    .intersection(inner);
    frame.render_widget(
        Paragraph::new(buttons).alignment(ratatui::layout::Alignment::Center),
        button_area,
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// 입력 처리
// ═══════════════════════════════════════════════════════════════════════════════

pub fn handle_input(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    if app.git_screen_state.is_none() {
        return;
    }

    // Check if we need to close - handle before borrowing state
    {
        let state = app.git_screen_state.as_ref().unwrap();
        let should_close = code == KeyCode::Esc
            && state.confirm_action.is_none()
            && state.input_mode.is_none()
            && !state.commit_input_active
            && state.log_detail.is_none();

        if should_close {
            app.current_screen = Screen::FilePanel;
            app.git_screen_state = None;
            app.refresh_panels();
            return;
        }
    }

    let state = app.git_screen_state.as_mut().unwrap();

    // Handle confirm dialog first
    if state.confirm_action.is_some() {
        handle_confirm_input(state, code);
        return;
    }

    // Handle input mode
    if state.input_mode.is_some() {
        handle_input_mode(state, code);
        return;
    }

    // Handle commit message input
    if state.commit_input_active && state.current_tab == GitTab::Commit {
        handle_commit_input(state, code, modifiers);
        return;
    }

    // Handle diff detail scrolling in log tab
    if state.log_detail.is_some() && state.current_tab == GitTab::Log {
        handle_log_detail_input(state, code);
        return;
    }

    // Handle diff detail scrolling in commit tab
    if state.log_detail.is_some() && state.current_tab == GitTab::Commit {
        handle_status_diff_input(state, code);
        return;
    }

    // Common keys
    match code {
        KeyCode::Char('1') => {
            state.current_tab = GitTab::Commit;
            state.refresh_status();
            return;
        }
        KeyCode::Char('2') => {
            state.current_tab = GitTab::Log;
            return;
        }
        KeyCode::Char('3') => {
            state.current_tab = GitTab::Branch;
            return;
        }
        KeyCode::Left => {
            state.current_tab = match state.current_tab {
                GitTab::Commit => GitTab::Branch,
                GitTab::Log => GitTab::Commit,
                GitTab::Branch => GitTab::Log,
            };
            if matches!(state.current_tab, GitTab::Commit) {
                state.refresh_status();
            }
            return;
        }
        KeyCode::Right => {
            state.current_tab = match state.current_tab {
                GitTab::Commit => GitTab::Log,
                GitTab::Log => GitTab::Branch,
                GitTab::Branch => GitTab::Commit,
            };
            if matches!(state.current_tab, GitTab::Commit) {
                state.refresh_status();
            }
            return;
        }
        _ => {}
    }

    // Tab-specific keys
    match state.current_tab {
        GitTab::Commit => handle_commit_tab_input(state, code, modifiers),
        GitTab::Log => handle_log_input(state, code),
        GitTab::Branch => handle_branch_input(state, code),
    }
}

fn handle_status_diff_input(state: &mut GitScreenState, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            state.log_detail = None;
            state.log_detail_scroll = 0;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.log_detail_scroll = state.log_detail_scroll.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.log_detail_scroll += 1;
        }
        KeyCode::PageUp => {
            state.log_detail_scroll = state.log_detail_scroll.saturating_sub(20);
        }
        KeyCode::PageDown => {
            state.log_detail_scroll += 20;
        }
        KeyCode::Home => {
            state.log_detail_scroll = 0;
        }
        KeyCode::End => {
            if let Some(ref detail) = state.log_detail {
                let total = detail.lines().count();
                state.log_detail_scroll = total;
            }
        }
        _ => {}
    }
}

fn handle_commit_tab_input(state: &mut GitScreenState, code: KeyCode, modifiers: KeyModifiers) {
    let len = state.status_files.len();
    if len == 0 && code != KeyCode::Tab {
        return;
    }

    // Ctrl+A: stage/unstage all
    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('a') {
        let has_unstaged = state.status_files.iter().any(|f| !f.staged);
        for entry in &state.status_files {
            if has_unstaged && !entry.staged {
                let _ = stage_file(&state.repo_path, &entry.path);
            } else if !has_unstaged && entry.staged {
                let _ = unstage_file(&state.repo_path, &entry.path);
            }
        }
        state.refresh_status();
        return;
    }

    match code {
        KeyCode::Up => {
            state.commit_selected = state.commit_selected.saturating_sub(1);
        }
        KeyCode::Down => {
            if len > 0 && state.commit_selected + 1 < len {
                state.commit_selected += 1;
            }
        }
        KeyCode::Home => {
            state.commit_selected = 0;
        }
        KeyCode::End => {
            if len > 0 {
                state.commit_selected = len.saturating_sub(1);
            }
        }
        KeyCode::PageUp => {
            state.commit_selected = state.commit_selected.saturating_sub(10);
        }
        KeyCode::PageDown => {
            if len > 0 {
                state.commit_selected = (state.commit_selected + 10).min(len.saturating_sub(1));
            }
        }
        KeyCode::Char(' ') => {
            // Stage/unstage toggle
            if let Some(entry) = state.status_files.get(state.commit_selected) {
                let path = entry.path.clone();
                if entry.staged {
                    let _ = unstage_file(&state.repo_path, &path);
                } else {
                    let _ = stage_file(&state.repo_path, &path);
                }
                state.refresh_status();
            }
        }
        KeyCode::Enter => {
            // Show diff for selected file
            if let Some(entry) = state.status_files.get(state.commit_selected) {
                let diff = get_file_diff(&state.repo_path, &entry.path, entry.staged);
                if diff.is_empty() {
                    let full_path = state.repo_path.join(&entry.path);
                    if let Ok(content) = std::fs::read_to_string(&full_path) {
                        state.log_detail = Some(content);
                    } else {
                        state.show_msg("Cannot display file");
                    }
                } else {
                    state.log_detail = Some(diff);
                }
                state.log_detail_scroll = 0;
            }
        }
        KeyCode::Tab => {
            state.commit_input_active = true;
        }
        KeyCode::Char('a') => {
            // Amend mode
            state.input_mode = Some(InputMode::CommitAmend);
            // Pre-fill with last commit message
            let output = git_cmd(&state.repo_path)
                .args(["log", "-1", "--format=%s"])
                .output();
            if let Ok(o) = output {
                if o.status.success() {
                    state.input_buffer = String::from_utf8_lossy(&o.stdout).trim().to_string();
                }
            }
        }
        _ => {}
    }
}

fn handle_commit_input(state: &mut GitScreenState, code: KeyCode, _modifiers: KeyModifiers) {
    match code {
        KeyCode::Tab | KeyCode::Esc => {
            state.commit_input_active = false;
        }
        KeyCode::Enter => {
            if state.commit_message.trim().is_empty() {
                state.show_msg("Commit message is empty");
                return;
            }
            match do_commit(&state.repo_path, &state.commit_message) {
                Ok(msg) => {
                    let short_msg = msg.lines().next().unwrap_or("Committed").to_string();
                    state.show_msg(&short_msg);
                    state.commit_message.clear();
                    state.commit_input_active = false;
                    state.refresh_all();
                }
                Err(e) => {
                    let short_err = e.lines().next().unwrap_or("Commit failed").to_string();
                    state.show_msg(&short_err);
                }
            }
        }
        KeyCode::Char(c) => {
            state.commit_message.push(c);
        }
        KeyCode::Backspace => {
            state.commit_message.pop();
        }
        _ => {}
    }
}

fn handle_log_input(state: &mut GitScreenState, code: KeyCode) {
    let len = state.log_entries.len();
    if len == 0 {
        return;
    }

    match code {
        KeyCode::Up => {
            state.log_selected = state.log_selected.saturating_sub(1);
        }
        KeyCode::Down => {
            if state.log_selected + 1 < len {
                state.log_selected += 1;
            }
        }
        KeyCode::Home => {
            state.log_selected = 0;
        }
        KeyCode::End => {
            state.log_selected = len.saturating_sub(1);
        }
        KeyCode::PageUp => {
            state.log_selected = state.log_selected.saturating_sub(10);
        }
        KeyCode::PageDown => {
            state.log_selected = (state.log_selected + 10).min(len.saturating_sub(1));
        }
        KeyCode::Enter => {
            // Show commit diff
            if let Some(entry) = state.log_entries.get(state.log_selected) {
                let diff = get_commit_diff(&state.repo_path, &entry.hash);
                state.log_detail = Some(diff);
                state.log_detail_scroll = 0;
            }
        }
        KeyCode::Char('r') => {
            if let Some(entry) = state.log_entries.get(state.log_selected) {
                state.confirm_action = Some(ConfirmAction::RestoreToCommit(entry.hash.clone()));
                state.confirm_selected_button = 1;
            }
        }
        _ => {}
    }
}

fn handle_log_detail_input(state: &mut GitScreenState, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            state.log_detail = None;
            state.log_detail_scroll = 0;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.log_detail_scroll = state.log_detail_scroll.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.log_detail_scroll += 1;
        }
        KeyCode::PageUp => {
            state.log_detail_scroll = state.log_detail_scroll.saturating_sub(20);
        }
        KeyCode::PageDown => {
            state.log_detail_scroll += 20;
        }
        KeyCode::Home => {
            state.log_detail_scroll = 0;
        }
        KeyCode::End => {
            if let Some(ref detail) = state.log_detail {
                let total = detail.lines().count();
                state.log_detail_scroll = total;
            }
        }
        // Allow selecting different commits while detail is open
        KeyCode::Left => {
            state.log_selected = state.log_selected.saturating_sub(1);
            if let Some(entry) = state.log_entries.get(state.log_selected) {
                let diff = get_commit_diff(&state.repo_path, &entry.hash);
                state.log_detail = Some(diff);
                state.log_detail_scroll = 0;
            }
        }
        KeyCode::Right => {
            if state.log_selected + 1 < state.log_entries.len() {
                state.log_selected += 1;
            }
            if let Some(entry) = state.log_entries.get(state.log_selected) {
                let diff = get_commit_diff(&state.repo_path, &entry.hash);
                state.log_detail = Some(diff);
                state.log_detail_scroll = 0;
            }
        }
        _ => {}
    }
}

fn handle_branch_input(state: &mut GitScreenState, code: KeyCode) {
    let len = state.branches.len();
    if len == 0 && code != KeyCode::Char('n') {
        return;
    }

    match code {
        KeyCode::Up => {
            state.branch_selected = state.branch_selected.saturating_sub(1);
        }
        KeyCode::Down => {
            if len > 0 && state.branch_selected + 1 < len {
                state.branch_selected += 1;
            }
        }
        KeyCode::Home => {
            state.branch_selected = 0;
        }
        KeyCode::End => {
            if len > 0 {
                state.branch_selected = len.saturating_sub(1);
            }
        }
        KeyCode::PageUp => {
            state.branch_selected = state.branch_selected.saturating_sub(10);
        }
        KeyCode::PageDown => {
            if len > 0 {
                state.branch_selected = (state.branch_selected + 10).min(len.saturating_sub(1));
            }
        }
        KeyCode::Enter | KeyCode::Char('c') => {
            // Checkout branch
            if let Some(branch) = state.branches.get(state.branch_selected) {
                if branch.is_current {
                    state.show_msg("Already on this branch");
                    return;
                }
                let name = branch.name.clone();
                match checkout_branch(&state.repo_path, &name) {
                    Ok(()) => {
                        state.show_msg(&format!("Switched to {}", name));
                        state.refresh_all();
                    }
                    Err(e) => {
                        let short_err = e.lines().next().unwrap_or("Checkout failed").to_string();
                        state.show_msg(&short_err);
                    }
                }
            }
        }
        KeyCode::Char('n') => {
            // New branch
            state.input_mode = Some(InputMode::BranchCreate);
            state.input_buffer.clear();
        }
        KeyCode::Char('x') => {
            // Delete branch (force)
            if let Some(branch) = state.branches.get(state.branch_selected) {
                if branch.is_current {
                    state.show_msg("Cannot delete current branch");
                    return;
                }
                if branch.is_remote {
                    state.show_msg("Cannot delete remote branch");
                    return;
                }
                state.confirm_action = Some(ConfirmAction::BranchDelete(branch.name.clone()));
                state.confirm_selected_button = 1;
            }
        }
        _ => {}
    }
}

fn handle_input_mode(state: &mut GitScreenState, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            state.input_mode = None;
            state.input_buffer.clear();
        }
        KeyCode::Enter => {
            let input = state.input_buffer.clone();
            if input.trim().is_empty() {
                state.input_mode = None;
                state.input_buffer.clear();
                return;
            }

            match state.input_mode.take() {
                Some(InputMode::BranchCreate) => {
                    match create_branch(&state.repo_path, input.trim()) {
                        Ok(()) => {
                            state.show_msg(&format!("Created branch {}", input.trim()));
                            state.refresh_all();
                        }
                        Err(e) => {
                            let short_err = e.lines().next().unwrap_or("Failed").to_string();
                            state.show_msg(&short_err);
                        }
                    }
                }
                Some(InputMode::CommitAmend) => {
                    match do_commit_amend(&state.repo_path, input.trim()) {
                        Ok(msg) => {
                            let short_msg = msg.lines().next().unwrap_or("Amended").to_string();
                            state.show_msg(&short_msg);
                            state.refresh_all();
                        }
                        Err(e) => {
                            let short_err = e.lines().next().unwrap_or("Amend failed").to_string();
                            state.show_msg(&short_err);
                        }
                    }
                }
                None => {}
            }
            state.input_buffer.clear();
        }
        KeyCode::Char(c) => {
            state.input_buffer.push(c);
        }
        KeyCode::Backspace => {
            state.input_buffer.pop();
        }
        _ => {}
    }
}

fn execute_confirm_action(state: &mut GitScreenState) {
    if let Some(action) = state.confirm_action.take() {
        match action {
            ConfirmAction::BranchDelete(name) => {
                match force_delete_branch(&state.repo_path, &name) {
                    Ok(()) => {
                        state.show_msg(&format!("Deleted branch {}", name));
                        state.refresh_all();
                    }
                    Err(e) => {
                        let short_err = e.lines().next().unwrap_or("Delete failed").to_string();
                        state.show_msg(&short_err);
                    }
                }
            }
            ConfirmAction::RestoreToCommit(hash) => {
                match restore_to_commit(&state.repo_path, &hash) {
                    Ok(msg) => {
                        state.show_msg(&msg);
                        state.refresh_all();
                    }
                    Err(e) => {
                        let short_err = e.lines().next().unwrap_or("Restore failed").to_string();
                        state.show_msg(&short_err);
                    }
                }
            }
        }
    }
}

fn handle_confirm_input(state: &mut GitScreenState, code: KeyCode) {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            execute_confirm_action(state);
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            state.confirm_action = None;
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
            state.confirm_selected_button = 1 - state.confirm_selected_button;
        }
        KeyCode::Enter => {
            if state.confirm_selected_button == 0 {
                execute_confirm_action(state);
            } else {
                state.confirm_action = None;
            }
        }
        _ => {}
    }
}

pub fn handle_paste(state: &mut GitScreenState, text: &str) {
    if state.commit_input_active {
        state.commit_message.push_str(text);
    } else if state.input_mode.is_some() {
        state.input_buffer.push_str(text);
    }
}
