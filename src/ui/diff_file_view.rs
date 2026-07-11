use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

use unicode_width::UnicodeWidthChar;

use super::app::App;
use super::theme::Theme;

// ═══════════════════════════════════════════════════════════════════════════════
// Data structures
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineStatus {
    Same,
    Modified,
    LeftOnly,
    RightOnly,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub left_line_no: Option<usize>,
    pub left_content: Option<String>,
    pub right_line_no: Option<usize>,
    pub right_content: Option<String>,
    pub line_status: DiffLineStatus,
}

pub struct DiffFileViewState {
    pub left_path: PathBuf,
    pub right_path: PathBuf,
    pub diff_lines: Vec<DiffLine>,
    pub scroll: usize, // visual row offset (0-based)
    pub visible_height: usize,
    pub left_total_lines: usize,
    pub right_total_lines: usize,
    pub change_positions: Vec<usize>,
    pub current_change: usize,
    pub file_name: String,
    pub max_scroll: usize,                 // max visual row offset
    pub change_visual_offsets: Vec<usize>, // visual row offset for each change_positions entry
}

// ═══════════════════════════════════════════════════════════════════════════════
// Binary detection
// ═══════════════════════════════════════════════════════════════════════════════

/// Check if raw bytes represent a binary file by looking for null bytes in the first 8KB.
fn is_binary(data: &[u8]) -> bool {
    let check_len = data.len().min(8192);
    data[..check_len].contains(&0)
}

// ═══════════════════════════════════════════════════════════════════════════════
// LCS diff algorithm
// ═══════════════════════════════════════════════════════════════════════════════

/// Compute LCS (Longest Common Subsequence) of two line sequences.
/// Returns a list of matched pairs (left_index, right_index).
///
/// Uses standard O(n*m) DP only while the matrix remains reasonably sized.
/// Larger inputs fall back to a linear-memory greedy matcher.
fn compute_lcs(left: &[String], right: &[String]) -> Vec<(usize, usize)> {
    let n = left.len();
    let m = right.len();

    // A 10k x 10k input used to allocate roughly 400 MiB even though the
    // combined-line threshold considered it acceptable.  Bound the actual DP
    // cell count instead (u32 is four bytes per cell).
    const MAX_LCS_CELLS: usize = 4_000_000;
    let cells = n
        .checked_add(1)
        .and_then(|rows| m.checked_add(1).and_then(|cols| rows.checked_mul(cols)));
    if cells.is_none_or(|cells| cells > MAX_LCS_CELLS) {
        return compute_lcs_simple(left, right);
    }

    // Standard O(n*m) DP approach
    // dp[i][j] = length of LCS of left[0..i] and right[0..j]
    let mut dp = vec![vec![0u32; m + 1]; n + 1];

    for i in 1..=n {
        for j in 1..=m {
            if left[i - 1] == right[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    // Backtrack to find actual LCS pairs
    let mut result = Vec::new();
    let mut i = n;
    let mut j = m;
    while i > 0 && j > 0 {
        if left[i - 1] == right[j - 1] {
            result.push((i - 1, j - 1));
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }

    result.reverse();
    result
}

/// Greedy matching for large files using indexed right-hand positions.
fn compute_lcs_simple(left: &[String], right: &[String]) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let mut right_pos = 0;
    let mut right_positions: HashMap<&str, VecDeque<usize>> = HashMap::new();

    for (index, line) in right.iter().enumerate() {
        right_positions
            .entry(line.as_str())
            .or_default()
            .push_back(index);
    }

    for (li, left_line) in left.iter().enumerate() {
        let Some(positions) = right_positions.get_mut(left_line.as_str()) else {
            continue;
        };
        while positions.front().is_some_and(|index| *index < right_pos) {
            positions.pop_front();
        }
        if let Some(ri) = positions.pop_front() {
            result.push((li, ri));
            right_pos = ri + 1;
        }
    }

    result
}

/// Build diff lines from two line sequences using LCS matching.
fn build_diff_lines(
    left_lines: &[String],
    right_lines: &[String],
    lcs: &[(usize, usize)],
) -> (Vec<DiffLine>, Vec<usize>) {
    let mut diff_lines = Vec::new();
    let mut change_positions = Vec::new();

    let mut li = 0usize; // current position in left
    let mut ri = 0usize; // current position in right

    for &(lcs_li, lcs_ri) in lcs {
        // Process gap before this LCS match
        // Lines in left[li..lcs_li] and right[ri..lcs_ri] are not in LCS
        let left_gap = &left_lines[li..lcs_li];
        let right_gap = &right_lines[ri..lcs_ri];

        emit_gap(
            left_gap,
            right_gap,
            &mut li,
            &mut ri,
            &mut diff_lines,
            &mut change_positions,
        );

        // Emit the matching line
        diff_lines.push(DiffLine {
            left_line_no: Some(li + 1),
            left_content: Some(left_lines[lcs_li].clone()),
            right_line_no: Some(ri + 1),
            right_content: Some(right_lines[lcs_ri].clone()),
            line_status: DiffLineStatus::Same,
        });
        li = lcs_li + 1;
        ri = lcs_ri + 1;
    }

    // Process remaining lines after last LCS match
    let left_gap = &left_lines[li..];
    let right_gap = &right_lines[ri..];
    emit_gap(
        left_gap,
        right_gap,
        &mut li,
        &mut ri,
        &mut diff_lines,
        &mut change_positions,
    );

    (diff_lines, change_positions)
}

/// Emit diff lines for a gap between LCS matches.
/// Pairs up lines as Modified where both sides have content,
/// then emits remaining as LeftOnly or RightOnly.
fn emit_gap(
    left_gap: &[String],
    right_gap: &[String],
    li: &mut usize,
    ri: &mut usize,
    diff_lines: &mut Vec<DiffLine>,
    change_positions: &mut Vec<usize>,
) {
    let paired = left_gap.len().min(right_gap.len());

    for idx in 0..paired {
        let pos = diff_lines.len();
        if change_positions.last() != Some(&pos) {
            // Mark first line of a contiguous change block
            if idx == 0 {
                change_positions.push(pos);
            }
        }
        diff_lines.push(DiffLine {
            left_line_no: Some(*li + 1),
            left_content: Some(left_gap[idx].clone()),
            right_line_no: Some(*ri + 1),
            right_content: Some(right_gap[idx].clone()),
            line_status: DiffLineStatus::Modified,
        });
        *li += 1;
        *ri += 1;
    }

    // Remaining left-only lines
    for idx in paired..left_gap.len() {
        let pos = diff_lines.len();
        if paired == 0 && idx == 0 {
            change_positions.push(pos);
        } else if idx == paired && paired > 0 {
            // Already recorded by the modified block above
        } else if idx == paired {
            change_positions.push(pos);
        }
        diff_lines.push(DiffLine {
            left_line_no: Some(*li + 1),
            left_content: Some(left_gap[idx].clone()),
            right_line_no: None,
            right_content: None,
            line_status: DiffLineStatus::LeftOnly,
        });
        *li += 1;
    }

    // Remaining right-only lines
    for idx in paired..right_gap.len() {
        let pos = diff_lines.len();
        if paired == 0 && left_gap.is_empty() && idx == 0 {
            change_positions.push(pos);
        } else if idx == paired && paired > 0 {
            // Already recorded
        } else if idx == paired && left_gap.is_empty() {
            change_positions.push(pos);
        }
        diff_lines.push(DiffLine {
            left_line_no: None,
            left_content: None,
            right_line_no: Some(*ri + 1),
            right_content: Some(right_gap[idx].clone()),
            line_status: DiffLineStatus::RightOnly,
        });
        *ri += 1;
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// DiffFileViewState implementation
// ═══════════════════════════════════════════════════════════════════════════════

impl DiffFileViewState {
    pub fn new(left_path: PathBuf, right_path: PathBuf, file_name: String) -> Self {
        // Guard against reading multi-GB files fully into memory (the text path below
        // also duplicates the contents several times). Mirror the file viewer's cap.
        const MAX_DIFF_FILE_BYTES: u64 = 100 * 1024 * 1024;
        let file_too_large = |p: &PathBuf| {
            fs::metadata(p)
                .map(|m| m.len() > MAX_DIFF_FILE_BYTES)
                .unwrap_or(false)
        };
        if file_too_large(&left_path) || file_too_large(&right_path) {
            let diff_lines = vec![DiffLine {
                left_line_no: None,
                left_content: Some("File too large to diff".to_string()),
                right_line_no: None,
                right_content: Some("File too large to diff".to_string()),
                line_status: DiffLineStatus::Same,
            }];
            return Self {
                left_path,
                right_path,
                diff_lines,
                scroll: 0,
                visible_height: 0,
                left_total_lines: 0,
                right_total_lines: 0,
                change_positions: Vec::new(),
                current_change: 0,
                file_name,
                max_scroll: 0,
                change_visual_offsets: Vec::new(),
            };
        }

        let left_data = fs::read(&left_path).ok();
        let right_data = fs::read(&right_path).ok();

        // Check for binary files
        let left_is_binary = left_data.as_ref().map_or(false, |d| is_binary(d));
        let right_is_binary = right_data.as_ref().map_or(false, |d| is_binary(d));

        if left_is_binary || right_is_binary {
            // Binary file: show a single informational line
            let diff_lines = vec![DiffLine {
                left_line_no: None,
                left_content: Some("Binary file".to_string()),
                right_line_no: None,
                right_content: Some("Binary file".to_string()),
                line_status: DiffLineStatus::Same,
            }];
            return Self {
                left_path,
                right_path,
                diff_lines,
                scroll: 0,
                visible_height: 0,
                left_total_lines: 0,
                right_total_lines: 0,
                change_positions: Vec::new(),
                current_change: 0,
                file_name,
                max_scroll: 0,
                change_visual_offsets: Vec::new(),
            };
        }

        // Read as text, handle missing files gracefully
        let left_text = left_data
            .map(|d| String::from_utf8_lossy(&d).into_owned())
            .unwrap_or_default();
        let right_text = right_data
            .map(|d| String::from_utf8_lossy(&d).into_owned())
            .unwrap_or_default();

        let left_lines: Vec<String> = if left_text.is_empty() {
            Vec::new()
        } else {
            left_text.lines().map(|l| l.to_string()).collect()
        };
        let right_lines: Vec<String> = if right_text.is_empty() {
            Vec::new()
        } else {
            right_text.lines().map(|l| l.to_string()).collect()
        };

        let left_total_lines = left_lines.len();
        let right_total_lines = right_lines.len();

        // Handle case where one file doesn't exist (all LeftOnly or RightOnly)
        let (diff_lines, change_positions) = if left_lines.is_empty() && !right_lines.is_empty() {
            let mut diffs = Vec::new();
            let mut changes = Vec::new();
            if !right_lines.is_empty() {
                changes.push(0);
            }
            for (idx, line) in right_lines.iter().enumerate() {
                diffs.push(DiffLine {
                    left_line_no: None,
                    left_content: None,
                    right_line_no: Some(idx + 1),
                    right_content: Some(line.clone()),
                    line_status: DiffLineStatus::RightOnly,
                });
            }
            (diffs, changes)
        } else if !left_lines.is_empty() && right_lines.is_empty() {
            let mut diffs = Vec::new();
            let mut changes = Vec::new();
            if !left_lines.is_empty() {
                changes.push(0);
            }
            for (idx, line) in left_lines.iter().enumerate() {
                diffs.push(DiffLine {
                    left_line_no: Some(idx + 1),
                    left_content: Some(line.clone()),
                    right_line_no: None,
                    right_content: None,
                    line_status: DiffLineStatus::LeftOnly,
                });
            }
            (diffs, changes)
        } else {
            // Both files have content: compute LCS-based diff
            let lcs = compute_lcs(&left_lines, &right_lines);
            build_diff_lines(&left_lines, &right_lines, &lcs)
        };

        Self {
            left_path,
            right_path,
            diff_lines,
            scroll: 0,
            visible_height: 0,
            left_total_lines,
            right_total_lines,
            change_positions,
            current_change: 0,
            file_name,
            max_scroll: 0,
            change_visual_offsets: Vec::new(),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Drawing
// ═══════════════════════════════════════════════════════════════════════════════

pub fn draw(
    frame: &mut Frame,
    state: &mut DiffFileViewState,
    area: Rect,
    theme: &Theme,
    kb: &crate::keybindings::Keybindings,
) {
    if area.height < 4 {
        return;
    }

    // Layout: Header(1) + Content(fill) + StatusBar(1) + FunctionBar(1)
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Header
            Constraint::Min(3),    // Content
            Constraint::Length(1), // StatusBar
            Constraint::Length(1), // FunctionBar
        ])
        .split(area);

    let header_area = layout[0];
    let content_area = layout[1];
    let status_area = layout[2];
    let function_area = layout[3];

    // Update visible height
    state.visible_height = content_area.height as usize;

    // ─── Header ─────────────────────────────────────────────────────────────
    let header_text = format!("[FILE DIFF] {}", state.file_name);
    let header_line = Line::from(Span::styled(
        header_text,
        Style::default()
            .fg(theme.diff_file_view.header_text)
            .bg(theme.diff_file_view.bg),
    ));
    let header_paragraph =
        Paragraph::new(header_line).style(Style::default().bg(theme.diff_file_view.bg));
    frame.render_widget(header_paragraph, header_area);

    // ─── Content: split 50:50 horizontal ────────────────────────────────────
    let content_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(content_area);

    let left_area = content_layout[0];
    let right_area = content_layout[1];
    // Left panel has Borders::RIGHT which consumes 1 column
    let left_inner_width = (left_area.width as usize).saturating_sub(1);

    let visible_lines = state.visible_height;
    let total_logical = state.diff_lines.len();

    // Line number width: marker(1) + digits + separator(│)
    let max_line = state.left_total_lines.max(state.right_total_lines).max(1);
    let digit_width = ((max_line as f64).log10().floor() as usize) + 1;
    let line_no_width = 1 + digit_width; // 1 for marker + digits

    let left_content_width = left_inner_width.saturating_sub(line_no_width + 1);
    let right_content_width = (right_area.width as usize).saturating_sub(line_no_width + 1);
    // Reserve 1 column on the right panel for the scrollbar (VerticalRight overlaps right_area).
    // Use the narrower width for wrapping so both sides break at the same position.
    let wrap_width = left_content_width.min(right_content_width.saturating_sub(1));

    // Compute visual row count for each logical line and cumulative offsets
    // visual_row_offsets[i] = total visual rows before logical line i
    let mut visual_row_offsets: Vec<usize> = Vec::with_capacity(total_logical + 1);
    let mut total_visual_rows: usize = 0;
    for dl in &state.diff_lines {
        visual_row_offsets.push(total_visual_rows);
        let visual_rows = if dl.line_status == DiffLineStatus::Modified {
            let lc = dl.left_content.as_deref().unwrap_or("");
            let rc = dl.right_content.as_deref().unwrap_or("");
            let left_exp = expand_chars(lc);
            let right_exp = expand_chars(rc);
            let max_len = left_exp.len().max(right_exp.len());
            count_inline_wrapped_rows(&left_exp, max_len, wrap_width)
                .max(count_inline_wrapped_rows(&right_exp, max_len, wrap_width))
        } else {
            let left_rows = match &dl.left_content {
                Some(c) if dl.line_status != DiffLineStatus::RightOnly => {
                    count_wrapped_lines(c, wrap_width)
                }
                _ => 1,
            };
            let right_rows = match &dl.right_content {
                Some(c) if dl.line_status != DiffLineStatus::LeftOnly => {
                    count_wrapped_lines(c, wrap_width)
                }
                _ => 1,
            };
            left_rows.max(right_rows)
        };
        total_visual_rows += visual_rows;
    }
    visual_row_offsets.push(total_visual_rows);

    // scroll is now a visual row offset; max_scroll = total - visible
    state.max_scroll = total_visual_rows.saturating_sub(visible_lines);

    // Compute change_visual_offsets for n/p navigation
    state.change_visual_offsets = state
        .change_positions
        .iter()
        .map(|&pos| {
            if pos < visual_row_offsets.len() {
                visual_row_offsets[pos]
            } else {
                0
            }
        })
        .collect();

    // Clamp scroll
    if state.scroll > state.max_scroll {
        state.scroll = state.max_scroll;
    }

    // Convert visual scroll position to logical line + skip_rows
    // Find the logical line that contains visual row state.scroll
    let start_logical = if total_logical == 0 {
        0
    } else {
        // Binary search: find largest i where visual_row_offsets[i] <= state.scroll
        let mut lo = 0usize;
        let mut hi = total_logical.saturating_sub(1);
        while lo < hi {
            let mid = lo + (hi - lo + 1) / 2;
            if visual_row_offsets[mid] <= state.scroll {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    };
    let skip_rows = state
        .scroll
        .saturating_sub(visual_row_offsets[start_logical]);

    // Build left and right display lines
    let mut left_lines_display: Vec<Line> = Vec::with_capacity(visible_lines);
    let mut right_lines_display: Vec<Line> = Vec::with_capacity(visible_lines);

    let current_change_pos = if !state.change_positions.is_empty() {
        Some(state.change_positions[state.current_change])
    } else {
        None
    };

    let mut visual_rows_filled = 0usize;
    let mut logical_idx = start_logical;
    let mut first_line = true;

    while visual_rows_filled < visible_lines && logical_idx < total_logical {
        let diff_line = &state.diff_lines[logical_idx];
        let is_current_change = current_change_pos == Some(logical_idx);
        let rows = render_diff_line(
            diff_line,
            line_no_width,
            left_inner_width,
            right_area.width as usize,
            wrap_width,
            theme,
            is_current_change,
        );
        let rows_to_skip = if first_line { skip_rows } else { 0 };
        first_line = false;
        for (row_idx, (left_spans, right_spans)) in rows.into_iter().enumerate() {
            if row_idx < rows_to_skip {
                continue;
            }
            if visual_rows_filled >= visible_lines {
                break;
            }
            left_lines_display.push(Line::from(left_spans));
            right_lines_display.push(Line::from(right_spans));
            visual_rows_filled += 1;
        }
        logical_idx += 1;
    }

    // Fill remaining lines with empty bg
    while visual_rows_filled < visible_lines {
        left_lines_display.push(Line::from(Span::styled(
            " ".repeat(left_inner_width),
            Style::default().bg(theme.diff_file_view.bg),
        )));
        right_lines_display.push(Line::from(Span::styled(
            " ".repeat(right_area.width as usize),
            Style::default().bg(theme.diff_file_view.bg),
        )));
        visual_rows_filled += 1;
    }

    // Render left panel
    let left_block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(theme.diff_file_view.border));
    let left_inner = left_block.inner(left_area);
    frame.render_widget(left_block, left_area);
    let left_paragraph = Paragraph::new(left_lines_display);
    frame.render_widget(left_paragraph, left_inner);

    // Render right panel
    let right_paragraph = Paragraph::new(right_lines_display);
    frame.render_widget(right_paragraph, right_area);

    // Scrollbar
    if total_visual_rows > visible_lines {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        let mut scrollbar_state =
            ScrollbarState::new(total_visual_rows.saturating_sub(visible_lines))
                .position(state.scroll);
        frame.render_stateful_widget(scrollbar, content_area, &mut scrollbar_state);
    }

    // ─── StatusBar ──────────────────────────────────────────────────────────
    let changes_count = state.change_positions.len();
    let current_display = if changes_count > 0 {
        state.current_change + 1
    } else {
        0
    };
    let status_text = format!(
        " Lines: {}/{} Changes: {} [{}/{}]",
        state.left_total_lines,
        state.right_total_lines,
        changes_count,
        current_display,
        changes_count,
    );
    let status_line = Line::from(Span::styled(
        status_text,
        Style::default()
            .fg(theme.diff_file_view.status_bar_text)
            .bg(theme.diff_file_view.status_bar_bg),
    ));
    let status_paragraph =
        Paragraph::new(status_line).style(Style::default().bg(theme.diff_file_view.status_bar_bg));
    frame.render_widget(status_paragraph, status_area);

    // ─── FunctionBar (keybindings에서 동적으로) ────────────────────────────
    use crate::keybindings::DiffFileViewAction;
    let key_style = Style::default()
        .fg(theme.diff_file_view.footer_key)
        .bg(theme.diff_file_view.bg);
    let text_style = Style::default()
        .fg(theme.diff_file_view.footer_text)
        .bg(theme.diff_file_view.bg);
    let shortcuts: Vec<(String, &str)> = vec![
        (
            kb.diff_file_view_first_key(DiffFileViewAction::MoveUp)
                .to_string(),
            "scroll ",
        ),
        (
            kb.diff_file_view_first_key(DiffFileViewAction::PageUp)
                .to_string(),
            "page ",
        ),
        (
            kb.diff_file_view_first_key(DiffFileViewAction::NextChange)
                .to_string(),
            "next ",
        ),
        (
            kb.diff_file_view_first_key(DiffFileViewAction::PrevChange)
                .to_string(),
            "prev ",
        ),
        (
            kb.diff_file_view_first_key(DiffFileViewAction::Close)
                .to_string(),
            "back",
        ),
    ];
    let mut fn_spans = Vec::new();
    for (key, label) in &shortcuts {
        fn_spans.push(Span::styled(key.as_str(), key_style));
        fn_spans.push(Span::styled(":", text_style));
        fn_spans.push(Span::styled(*label, text_style));
    }
    let fn_line = Line::from(fn_spans);
    let fn_paragraph = Paragraph::new(fn_line).style(Style::default().bg(theme.diff_file_view.bg));
    frame.render_widget(fn_paragraph, function_area);
}

/// Render a single DiffLine into multiple visual rows of (left_spans, right_spans).
/// Wraps long lines at `wrap_width` (same for both sides) and pads to each panel's own width.
fn render_diff_line<'a>(
    diff_line: &DiffLine,
    line_no_width: usize,
    left_width: usize,
    right_width: usize,
    wrap_width: usize,
    theme: &Theme,
    is_current_change: bool,
) -> Vec<(Vec<Span<'a>>, Vec<Span<'a>>)> {
    let colors = &theme.diff_file_view;

    // Determine styles based on status
    let (left_style, right_style, left_empty, right_empty) = match diff_line.line_status {
        DiffLineStatus::Same => {
            let s = Style::default().fg(colors.same_text).bg(colors.bg);
            (s, s, false, false)
        }
        DiffLineStatus::Modified => {
            let s = Style::default()
                .fg(colors.modified_text)
                .bg(colors.modified_bg);
            (s, s, false, false)
        }
        DiffLineStatus::LeftOnly => {
            let ls = Style::default()
                .fg(colors.left_only_text)
                .bg(colors.left_only_bg);
            let rs = Style::default().bg(colors.empty_bg);
            (ls, rs, false, true)
        }
        DiffLineStatus::RightOnly => {
            let ls = Style::default().bg(colors.empty_bg);
            let rs = Style::default()
                .fg(colors.right_only_text)
                .bg(colors.right_only_bg);
            (ls, rs, true, false)
        }
    };

    let line_no_style = Style::default()
        .fg(colors.line_number)
        .bg(match diff_line.line_status {
            DiffLineStatus::Same => colors.bg,
            DiffLineStatus::Modified => colors.modified_bg,
            DiffLineStatus::LeftOnly => colors.left_only_bg,
            DiffLineStatus::RightOnly => colors.empty_bg,
        });

    let line_no_style_right =
        Style::default()
            .fg(colors.line_number)
            .bg(match diff_line.line_status {
                DiffLineStatus::Same => colors.bg,
                DiffLineStatus::Modified => colors.modified_bg,
                DiffLineStatus::LeftOnly => colors.empty_bg,
                DiffLineStatus::RightOnly => colors.right_only_bg,
            });

    // Current change marker
    let marker = if is_current_change { "\u{25B6}" } else { " " };
    let num_width = line_no_width.saturating_sub(1); // 1 char reserved for marker

    // Inline change style for character-level highlighting within Modified lines
    let inline_style = Style::default()
        .fg(colors.inline_change_text)
        .bg(colors.inline_change_bg);

    // Panel content widths (for padding), and wrap_width (for line breaking)
    let left_content_width = left_width.saturating_sub(line_no_width + 1);
    let right_content_width = right_width.saturating_sub(line_no_width + 1);
    let left_extra_pad = left_content_width.saturating_sub(wrap_width);
    let right_extra_pad = right_content_width.saturating_sub(wrap_width);

    // Build wrapped content rows for each side (wrapped at wrap_width)
    let left_content_rows: Vec<Vec<Span<'a>>>;
    let right_content_rows: Vec<Vec<Span<'a>>>;

    if diff_line.line_status == DiffLineStatus::Modified {
        let lc = diff_line.left_content.as_deref().unwrap_or("");
        let rc = diff_line.right_content.as_deref().unwrap_or("");
        left_content_rows =
            build_inline_wrapped_lines(lc, rc, wrap_width, left_style, inline_style);
        right_content_rows =
            build_inline_wrapped_lines(rc, lc, wrap_width, right_style, inline_style);
    } else {
        // Non-modified lines: wrap_content at wrap_width
        if left_empty {
            left_content_rows = vec![vec![Span::styled(
                " ".repeat(left_content_width),
                Style::default().bg(colors.empty_bg),
            )]];
        } else {
            let lc = diff_line.left_content.as_deref().unwrap_or("");
            let segments = wrap_content(lc, wrap_width);
            left_content_rows = segments
                .into_iter()
                .map(|s| vec![Span::styled(s, left_style)])
                .collect();
        }

        if right_empty {
            right_content_rows = vec![vec![Span::styled(
                " ".repeat(right_content_width),
                Style::default().bg(colors.empty_bg),
            )]];
        } else {
            let rc = diff_line.right_content.as_deref().unwrap_or("");
            let segments = wrap_content(rc, wrap_width);
            right_content_rows = segments
                .into_iter()
                .map(|s| vec![Span::styled(s, right_style)])
                .collect();
        }
    }

    let row_count = left_content_rows.len().max(right_content_rows.len());
    let mut result: Vec<(Vec<Span<'a>>, Vec<Span<'a>>)> = Vec::with_capacity(row_count);

    for row_idx in 0..row_count {
        // --- Left side prefix ---
        let left_prefix = if row_idx == 0 {
            if left_empty {
                let no_str = format!("{}{:>width$}\u{2502}", marker, "", width = num_width);
                Span::styled(
                    no_str,
                    Style::default().fg(colors.line_number).bg(colors.empty_bg),
                )
            } else {
                let no_str = match diff_line.left_line_no {
                    Some(n) => format!("{}{:>width$}\u{2502}", marker, n, width = num_width),
                    None => format!("{}{:>width$}\u{2502}", marker, "", width = num_width),
                };
                Span::styled(no_str, line_no_style)
            }
        } else {
            if left_empty {
                let no_str = format!("{:>width$}\u{2502}", "", width = line_no_width);
                Span::styled(
                    no_str,
                    Style::default().fg(colors.line_number).bg(colors.empty_bg),
                )
            } else {
                let no_str = format!("{:>width$}\u{2502}", "", width = line_no_width);
                Span::styled(no_str, line_no_style)
            }
        };

        // --- Right side prefix ---
        let right_prefix = if row_idx == 0 {
            if right_empty {
                let no_str = format!("{}{:>width$}\u{2502}", marker, "", width = num_width);
                Span::styled(
                    no_str,
                    Style::default().fg(colors.line_number).bg(colors.empty_bg),
                )
            } else {
                let no_str = match diff_line.right_line_no {
                    Some(n) => format!("{}{:>width$}\u{2502}", marker, n, width = num_width),
                    None => format!("{}{:>width$}\u{2502}", marker, "", width = num_width),
                };
                Span::styled(no_str, line_no_style_right)
            }
        } else {
            if right_empty {
                let no_str = format!("{:>width$}\u{2502}", "", width = line_no_width);
                Span::styled(
                    no_str,
                    Style::default().fg(colors.line_number).bg(colors.empty_bg),
                )
            } else {
                let no_str = format!("{:>width$}\u{2502}", "", width = line_no_width);
                Span::styled(no_str, line_no_style_right)
            }
        };

        // --- Left content (wrap_width) + extra padding to fill panel ---
        let left_content = if row_idx < left_content_rows.len() {
            let mut spans = left_content_rows[row_idx].clone();
            if left_extra_pad > 0 {
                spans.push(Span::styled(" ".repeat(left_extra_pad), left_style));
            }
            spans
        } else {
            let pad_style = if left_empty {
                Style::default().bg(colors.empty_bg)
            } else {
                left_style
            };
            vec![Span::styled(" ".repeat(left_content_width), pad_style)]
        };

        // --- Right content (wrap_width) + extra padding to fill panel ---
        let right_content = if row_idx < right_content_rows.len() {
            let mut spans = right_content_rows[row_idx].clone();
            if right_extra_pad > 0 {
                spans.push(Span::styled(" ".repeat(right_extra_pad), right_style));
            }
            spans
        } else {
            let pad_style = if right_empty {
                Style::default().bg(colors.empty_bg)
            } else {
                right_style
            };
            vec![Span::styled(" ".repeat(right_content_width), pad_style)]
        };

        // Assemble left spans: prefix + content
        let mut left_spans = vec![left_prefix];
        left_spans.extend(left_content);

        // Assemble right spans: prefix + content
        let mut right_spans = vec![right_prefix];
        right_spans.extend(right_content);

        result.push((left_spans, right_spans));
    }

    result
}

/// Count how many visual lines a string occupies when wrapped at `width` columns.
/// Handles CJK (2-column) characters and tab expansion (wrap-aware tab stops).
/// Must stay consistent with `wrap_content`.
fn count_wrapped_lines(content: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let mut lines = 1usize;
    let mut col = 0usize;

    for ch in content.chars() {
        if ch == '\t' {
            let spaces = 4 - (col % 4);
            for _ in 0..spaces {
                if col >= width {
                    lines += 1;
                    col = 0;
                }
                col += 1;
            }
        } else {
            let ch_w = ch.width().unwrap_or(0);
            if ch_w > 0 && col + ch_w > width {
                lines += 1;
                col = 0;
            }
            col += ch_w;
        }
    }
    lines
}

/// Wrap a string into segments of exactly `width` display columns each (space-padded).
/// Handles CJK boundary: if a 2-column char would overflow, pad current line and move char to next.
/// Tabs expanded to spaces (4-stop).
fn wrap_content(content: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut col = 0usize;

    for ch in content.chars() {
        if ch == '\t' {
            let spaces = 4 - (col % 4);
            for _ in 0..spaces {
                if col >= width {
                    // Pad and start new segment
                    while col < width {
                        current.push(' ');
                        col += 1;
                    }
                    segments.push(std::mem::take(&mut current));
                    col = 0;
                }
                current.push(' ');
                col += 1;
            }
        } else {
            let ch_w = ch.width().unwrap_or(0);
            if ch_w > 0 && col + ch_w > width {
                // Pad current line and start new
                while col < width {
                    current.push(' ');
                    col += 1;
                }
                segments.push(std::mem::take(&mut current));
                col = 0;
            }
            current.push(ch);
            col += ch_w;
        }
    }

    // Pad the last segment
    while col < width {
        current.push(' ');
        col += 1;
    }
    segments.push(current);

    segments
}

/// Count wrapped visual rows for inline-diff rendering of Modified lines.
/// Mirrors the wrapping logic of `build_inline_wrapped_lines` exactly:
/// processes `max_len` positions, using the actual char from `this_chars`
/// or a space if beyond its length.
fn count_inline_wrapped_rows(this_chars: &[char], max_len: usize, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let mut lines = 1usize;
    let mut col = 0usize;
    for i in 0..max_len {
        let ch = this_chars.get(i).copied().unwrap_or(' ');
        let ch_w = ch.width().unwrap_or(0);
        if ch_w > 0 && col + ch_w > width {
            lines += 1;
            col = 0;
        }
        col += ch_w;
    }
    lines
}

/// Expand a string into a flat character list with tabs expanded to spaces.
fn expand_chars(s: &str) -> Vec<char> {
    let mut result = Vec::new();
    let mut col = 0;
    for ch in s.chars() {
        if ch == '\t' {
            let spaces = 4 - (col % 4);
            for _ in 0..spaces {
                result.push(' ');
                col += 1;
            }
        } else {
            result.push(ch);
            col += ch.width().unwrap_or(0);
        }
    }
    result
}

/// Build wrapped visual lines for a Modified line with inline diff highlighting.
/// Returns Vec of span-rows, each row exactly `width` display columns (space-padded).
/// Wrapping semantics match `wrap_content`: a row is flushed only when the NEXT char
/// would overflow, not when exactly filling. This keeps row counts consistent with
/// `count_inline_wrapped_rows`.
fn build_inline_wrapped_lines<'a>(
    this_content: &str,
    other_content: &str,
    width: usize,
    base_style: Style,
    inline_style: Style,
) -> Vec<Vec<Span<'a>>> {
    if width == 0 {
        return vec![vec![]];
    }
    let this_chars = expand_chars(this_content);
    let other_chars = expand_chars(other_content);

    let max_len = this_chars.len().max(other_chars.len());

    let mut all_rows: Vec<Vec<Span<'a>>> = Vec::new();
    let mut row_spans: Vec<Span<'a>> = Vec::new();
    let mut buf = String::new();
    let mut buf_is_diff = false;
    let mut col = 0usize;

    for i in 0..max_len {
        let this_ch = this_chars.get(i).copied();
        let other_ch = other_chars.get(i).copied();
        let ch = this_ch.unwrap_or(' ');
        let ch_w = ch.width().unwrap_or(0);
        // Only highlight as diff when this side has actual content at this position.
        // Beyond this side's content, use base_style (padding spaces, not inline highlight).
        let is_diff = this_ch.is_some() && this_ch != other_ch;

        // Check if this char would overflow current row
        if ch_w > 0 && col + ch_w > width {
            // Flush buffer
            if !buf.is_empty() {
                row_spans.push(Span::styled(
                    std::mem::take(&mut buf),
                    if buf_is_diff {
                        inline_style
                    } else {
                        base_style
                    },
                ));
            }
            // Pad remainder (for CJK: col might be width-1 when a 2-col char doesn't fit)
            if col < width {
                row_spans.push(Span::styled(" ".repeat(width - col), base_style));
            }
            all_rows.push(std::mem::take(&mut row_spans));
            col = 0;
            buf_is_diff = is_diff;
        }

        // Style change → flush
        if is_diff != buf_is_diff && !buf.is_empty() {
            row_spans.push(Span::styled(
                std::mem::take(&mut buf),
                if buf_is_diff {
                    inline_style
                } else {
                    base_style
                },
            ));
        }
        buf_is_diff = is_diff;
        buf.push(ch);
        col += ch_w;
    }

    // Flush remaining buffer and pad final row
    if !buf.is_empty() {
        row_spans.push(Span::styled(
            buf,
            if buf_is_diff {
                inline_style
            } else {
                base_style
            },
        ));
    }
    if col < width {
        row_spans.push(Span::styled(" ".repeat(width - col), base_style));
    }
    // Always push the final row (contains at least padding for empty content)
    all_rows.push(row_spans);

    all_rows
}

// ═══════════════════════════════════════════════════════════════════════════════
// Input handling
// ═══════════════════════════════════════════════════════════════════════════════

pub fn handle_input(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    use crate::keybindings::DiffFileViewAction;

    let state = match app.diff_file_view_state.as_mut() {
        Some(s) => s,
        None => return,
    };

    let visible = state.visible_height;
    let max_scroll = state.max_scroll;

    if let Some(action) = app.keybindings.diff_file_view_action(code, modifiers) {
        match action {
            DiffFileViewAction::MoveUp => {
                state.scroll = state.scroll.saturating_sub(1);
            }
            DiffFileViewAction::MoveDown => {
                if state.scroll < max_scroll {
                    state.scroll += 1;
                }
            }
            DiffFileViewAction::PageUp => {
                state.scroll = state.scroll.saturating_sub(visible);
            }
            DiffFileViewAction::PageDown => {
                state.scroll = (state.scroll + visible).min(max_scroll);
            }
            DiffFileViewAction::GoHome => {
                state.scroll = 0;
            }
            DiffFileViewAction::GoEnd => {
                state.scroll = max_scroll;
            }
            DiffFileViewAction::NextChange => {
                if !state.change_positions.is_empty() {
                    if state.current_change + 1 < state.change_positions.len() {
                        state.current_change += 1;
                    }
                    if state.current_change < state.change_visual_offsets.len() {
                        let target = state.change_visual_offsets[state.current_change];
                        state.scroll = target.saturating_sub(visible / 4).min(max_scroll);
                    }
                }
            }
            DiffFileViewAction::PrevChange => {
                if !state.change_positions.is_empty() {
                    if state.current_change > 0 {
                        state.current_change -= 1;
                    }
                    if state.current_change < state.change_visual_offsets.len() {
                        let target = state.change_visual_offsets[state.current_change];
                        state.scroll = target.saturating_sub(visible / 4).min(max_scroll);
                    }
                }
            }
            DiffFileViewAction::Close => {
                app.current_screen = super::app::Screen::DiffScreen;
                app.diff_file_view_state = None;
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_lcs_identical() {
        let left = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let right = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let lcs = compute_lcs(&left, &right);
        assert_eq!(lcs, vec![(0, 0), (1, 1), (2, 2)]);
    }

    #[test]
    fn test_compute_lcs_different() {
        let left = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let right = vec!["x".to_string(), "y".to_string(), "z".to_string()];
        let lcs = compute_lcs(&left, &right);
        assert!(lcs.is_empty());
    }

    #[test]
    fn test_compute_lcs_partial() {
        let left = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let right = vec!["a".to_string(), "x".to_string(), "c".to_string()];
        let lcs = compute_lcs(&left, &right);
        assert_eq!(lcs, vec![(0, 0), (2, 2)]);
    }

    #[test]
    fn test_build_diff_lines_same() {
        let left = vec!["line1".to_string(), "line2".to_string()];
        let right = vec!["line1".to_string(), "line2".to_string()];
        let lcs = compute_lcs(&left, &right);
        let (diff_lines, changes) = build_diff_lines(&left, &right, &lcs);
        assert_eq!(diff_lines.len(), 2);
        assert_eq!(diff_lines[0].line_status, DiffLineStatus::Same);
        assert_eq!(diff_lines[1].line_status, DiffLineStatus::Same);
        assert!(changes.is_empty());
    }

    #[test]
    fn test_build_diff_lines_modified() {
        let left = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let right = vec!["a".to_string(), "x".to_string(), "c".to_string()];
        let lcs = compute_lcs(&left, &right);
        let (diff_lines, changes) = build_diff_lines(&left, &right, &lcs);
        assert_eq!(diff_lines.len(), 3);
        assert_eq!(diff_lines[0].line_status, DiffLineStatus::Same);
        assert_eq!(diff_lines[1].line_status, DiffLineStatus::Modified);
        assert_eq!(diff_lines[2].line_status, DiffLineStatus::Same);
        assert!(!changes.is_empty());
    }

    #[test]
    fn test_wrap_content() {
        // Short string: single segment, padded
        assert_eq!(wrap_content("hello", 10), vec!["hello     "]);
        // Exact fit
        assert_eq!(wrap_content("hello", 5), vec!["hello"]);
        // Wraps into two segments
        assert_eq!(
            wrap_content("hello world!", 5),
            vec!["hello", " worl", "d!   "]
        );
        // Empty string: padded
        assert_eq!(wrap_content("", 3), vec!["   "]);
    }

    #[test]
    fn test_count_wrapped_lines() {
        assert_eq!(count_wrapped_lines("hello", 10), 1);
        assert_eq!(count_wrapped_lines("hello", 5), 1);
        assert_eq!(count_wrapped_lines("hello world!", 5), 3);
        assert_eq!(count_wrapped_lines("", 5), 1);
    }

    #[test]
    fn test_is_binary() {
        assert!(is_binary(&[0x00, 0x01, 0x02]));
        assert!(!is_binary(b"hello world"));
        assert!(!is_binary(&[]));
    }

    #[test]
    fn test_compute_lcs_empty() {
        let left: Vec<String> = Vec::new();
        let right: Vec<String> = Vec::new();
        let lcs = compute_lcs(&left, &right);
        assert!(lcs.is_empty());
    }

    #[test]
    fn large_adversarial_diff_avoids_quadratic_matrix_and_scan() {
        let left = vec!["left-only".to_string(); 10_000];
        let right = vec!["right-only".to_string(); 10_000];

        assert!(compute_lcs(&left, &right).is_empty());
    }
}
