use std::collections::BTreeSet;
use unicode_width::UnicodeWidthChar;

use super::{EditAction, EditorState};

/// Prefix sums of rendered row counts. Scrolling reuses this layout; edits only
/// recount changed lines unless the document's line structure or width changed.
#[derive(Debug, Default)]
pub(super) struct WrapLayout {
    width: usize,
    tab_size: usize,
    row_offsets: Vec<usize>,
    dirty_lines: BTreeSet<usize>,
    valid: bool,
}

impl WrapLayout {
    pub(super) fn invalidate(&mut self, action: &EditAction) {
        if !self.valid {
            return;
        }
        match action {
            EditAction::Insert { line, .. }
            | EditAction::Delete { line, .. }
            | EditAction::Replace { line, .. } => {
                self.dirty_lines.insert(*line);
            }
            EditAction::SwapLines { line1, line2 } => {
                self.dirty_lines.insert(*line1);
                self.dirty_lines.insert(*line2);
            }
            EditAction::SetLineEnding { .. } => {}
            EditAction::Batch { actions } => {
                for action in actions {
                    self.invalidate(action);
                }
            }
            EditAction::InsertLine { .. }
            | EditAction::DeleteLine { .. }
            | EditAction::MergeLine { .. }
            | EditAction::SplitLine { .. } => {
                self.valid = false;
            }
        }
    }

    fn refresh(&mut self, lines: &[String], width: usize, tab_size: usize) {
        if !self.valid
            || self.width != width
            || self.tab_size != tab_size
            || self.row_offsets.len() != lines.len() + 1
        {
            self.row_offsets.clear();
            self.row_offsets.push(0);
            let mut offset = 0usize;
            for line in lines {
                offset = offset.saturating_add(wrapped_row_count(line, width, tab_size));
                self.row_offsets.push(offset);
            }
            self.width = width;
            self.tab_size = tab_size;
            self.valid = true;
        } else if let Some(&first) = self.dirty_lines.first().filter(|&&line| line < lines.len()) {
            let mut old_start = self.row_offsets[first];
            let mut offset = old_start;
            for (line_idx, line) in lines.iter().enumerate().skip(first) {
                let old_end = self.row_offsets[line_idx + 1];
                let rows = if self.dirty_lines.contains(&line_idx) {
                    wrapped_row_count(line, width, tab_size)
                } else {
                    old_end - old_start
                };
                offset = offset.saturating_add(rows);
                self.row_offsets[line_idx + 1] = offset;
                old_start = old_end;
            }
        }
        self.dirty_lines.clear();
    }
}

/// Count the same segments as tab expansion followed by compute_wrap_segments,
/// without allocating an expanded string or a mapping for every character.
pub(super) fn wrapped_row_count(line: &str, width: usize, tab_size: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let tab_size = tab_size.max(1);
    let mut rows = 1;
    let mut row_width = 0;
    let mut visual_col = 0;
    for ch in line.chars() {
        let (char_width, repeats) = if ch == '\t' {
            (1, tab_size - visual_col % tab_size)
        } else {
            (UnicodeWidthChar::width(ch).unwrap_or(1), 1)
        };
        for _ in 0..repeats {
            if row_width > 0 && row_width + char_width > width {
                rows += 1;
                row_width = char_width;
            } else {
                row_width += char_width;
            }
            visual_col += char_width;
        }
    }
    rows
}

impl EditorState {
    /// Normalize the viewport after edits/resizes without moving the caret or
    /// changing selection, and return (total rendered rows, first rendered row).
    pub(super) fn scrollbar_viewport(&mut self) -> (usize, usize) {
        if !self.word_wrap {
            self.wrap_scroll_offset = 0;
            self.scroll = self
                .scroll
                .min(self.lines.len().saturating_sub(self.visible_height));
            return (self.lines.len(), self.scroll);
        }

        self.wrap_layout
            .refresh(&self.lines, self.visible_width, self.tab_size);
        if self.lines.is_empty() {
            self.scroll = 0;
            self.wrap_scroll_offset = 0;
            return (0, 0);
        }

        let offsets = &self.wrap_layout.row_offsets;
        let total = offsets[self.lines.len()];
        self.scroll = self.scroll.min(self.lines.len() - 1);
        let top_line_rows = offsets[self.scroll + 1] - offsets[self.scroll];
        self.wrap_scroll_offset = self.wrap_scroll_offset.min(top_line_rows.saturating_sub(1));
        let top = offsets[self.scroll] + self.wrap_scroll_offset;
        let clamped_top = top.min(total.saturating_sub(self.visible_height));
        if clamped_top != top {
            self.scroll = offsets.partition_point(|&offset| offset <= clamped_top) - 1;
            self.wrap_scroll_offset = clamped_top - offsets[self.scroll];
        }
        (total, clamped_top)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::{Keybindings, KeybindingsConfig};
    use crate::ui::{file_editor::Selection, theme::Theme};
    use ratatui::{backend::TestBackend, Terminal};

    fn wrapped_editor(lines: &[&str]) -> EditorState {
        let mut state = EditorState::new();
        state.lines = lines.iter().map(|line| line.to_string()).collect();
        state.word_wrap = true;
        state.visible_width = 4;
        state.visible_height = 4;
        state
    }

    fn draw_bar(state: &mut EditorState, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = Theme::default();
        let kb = Keybindings::from_config(&KeybindingsConfig::default());
        terminal
            .draw(|frame| {
                let area = frame.area();
                super::super::draw(frame, state, area, &theme, &kb);
            })
            .unwrap();
        let area = state.mouse.area.unwrap();
        (area.y..area.bottom())
            .map(|y| {
                terminal.backend().buffer()[(area.right(), y)]
                    .symbol()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn row_counts_match_rendered_segments_for_tabs_unicode_and_narrow_widths() {
        let samples = [
            "",
            "abcdefghi",
            "가나a다",
            "a\t가\txyz",
            "e\u{301}\t한글",
            "\u{301}가a",
        ];
        for width in 0..10 {
            for tab_size in [1, 2, 4, 8] {
                let mut state = wrapped_editor(&samples);
                state.visible_width = width;
                state.tab_size = tab_size;
                for (index, line) in samples.iter().enumerate() {
                    let (expanded, _) = state.expand_tabs_with_mapping(line);
                    let expected = EditorState::compute_wrap_segments(&expanded, width).len();
                    assert_eq!(
                        state.count_wrapped_rows(index),
                        expected,
                        "{line:?}, width={width}, tab={tab_size}"
                    );
                }
            }
        }
    }

    #[test]
    fn wrapped_wheel_moves_the_scrollbar_within_one_logical_line_without_editing() {
        let mut state = wrapped_editor(&["abcdefghijklmnopqrstuvwxyz0123456789ABCD"]);
        state.selection = Some(Selection::new(0, 0));
        let selection = state.selection;
        let original = state.lines.clone();
        // Four text columns and six visible rows: one logical line overflows.
        let top = draw_bar(&mut state, 12, 10);
        assert_eq!(state.scrollbar_viewport(), (10, 0));
        state.scroll_mouse_rows(3);
        let middle = draw_bar(&mut state, 12, 10);
        assert_ne!(middle, top);
        state.scroll_mouse_rows(100);
        let bottom = draw_bar(&mut state, 12, 10);
        assert_eq!((state.scroll, state.wrap_scroll_offset), (0, 4));
        assert_eq!(state.scrollbar_viewport(), (10, 4));
        assert_ne!(bottom[1], top[1]);
        assert_eq!(bottom[bottom.len() - 2], top[1]);
        state.scroll_mouse_rows(-100);
        assert_eq!(draw_bar(&mut state, 12, 10), top);
        assert_eq!((state.cursor_line, state.cursor_col), (0, 0));
        assert_eq!(state.selection, selection);
        assert_eq!(state.lines, original);
        assert!(!state.modified);
        assert!(state.undo_stack.is_empty());
        assert!(state.redo_stack.is_empty());
    }

    #[test]
    fn resize_and_wrap_toggle_clamp_the_viewport_without_moving_the_caret() {
        let mut state = wrapped_editor(&["abcdefghijklmnopqrstuvwx", "yz"]);
        assert_eq!(state.scrollbar_viewport(), (7, 0));
        state.scroll = 1;
        assert_eq!(state.scrollbar_viewport(), (7, 3));
        assert_eq!((state.scroll, state.wrap_scroll_offset), (0, 3));
        state.visible_width = 8;
        assert_eq!(state.scrollbar_viewport(), (4, 0));
        assert_eq!((state.scroll, state.wrap_scroll_offset), (0, 0));
        state.visible_width = 4;
        state.scroll = 1;
        assert_eq!(state.scrollbar_viewport(), (7, 3));
        state.word_wrap = false;
        assert_eq!(state.scrollbar_viewport(), (2, 0));
        assert_eq!(state.wrap_scroll_offset, 0);
        assert_eq!((state.cursor_line, state.cursor_col), (0, 0));
        assert!(!state.modified);
    }

    #[test]
    fn edits_undo_redo_and_tab_changes_refresh_wrapped_row_totals() {
        let mut state = wrapped_editor(&["abcd", "x\ty", "efgh"]);
        assert_eq!(state.scrollbar_viewport().0, 4);
        state.cursor_col = 4;
        state.insert_char('e');
        assert_eq!(state.scrollbar_viewport().0, 5);
        state.undo();
        assert_eq!(state.scrollbar_viewport().0, 4);
        state.redo();
        assert_eq!(state.scrollbar_viewport().0, 5);
        state.tab_size = 8;
        assert_eq!(state.scrollbar_viewport().0, 6);
        state.cursor_line = 2;
        state.cursor_col = 2;
        state.insert_newline();
        assert_eq!(state.scrollbar_viewport().0, 7);
        state.undo();
        assert_eq!(state.scrollbar_viewport().0, 6);
        state.redo();
        assert_eq!(state.scrollbar_viewport().0, 7);
    }

    #[test]
    fn batch_edits_and_line_swaps_update_offsets_before_unchanged_lines() {
        let mut state = wrapped_editor(&["a", "abcdefgh", "z"]);
        state.visible_height = 1;
        state.scroll = 2;
        assert_eq!(state.scrollbar_viewport(), (4, 3));
        state.selection = Some(Selection {
            start_line: 0,
            start_col: 0,
            end_line: 1,
            end_col: 8,
        });
        state.indent();
        assert_eq!(state.scrollbar_viewport(), (6, 5));
        state.undo();
        state.scroll = 2;
        assert_eq!(state.scrollbar_viewport(), (4, 3));
        state.redo();
        state.scroll = 2;
        assert_eq!(state.scrollbar_viewport(), (6, 5));

        state.cursor_line = 1;
        state.cursor_col = 0;
        state.move_line_up();
        state.scroll = 1;
        assert_eq!(state.scrollbar_viewport(), (6, 3));

        // Edits made with wrapping disabled must invalidate its cached layout.
        state.word_wrap = false;
        state.insert_char('x');
        state.word_wrap = true;
        state.scroll = 1;
        assert_eq!(state.scrollbar_viewport(), (7, 4));
    }

    #[test]
    fn loading_another_file_with_the_same_line_count_resets_the_layout() {
        let mut state = wrapped_editor(&["abcdefghijklmnopqrstuvwx"]);
        assert_eq!(state.scrollbar_viewport().0, 6);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.txt");
        std::fs::write(&path, "a").unwrap();
        state.load_file(&path).unwrap();
        assert_eq!(state.scrollbar_viewport(), (1, 0));
    }
}
