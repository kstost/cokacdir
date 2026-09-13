//! Mouse coordinates refer to rendered text cells, including tab expansion and wrap segments.
use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub(crate) struct EditorMouseState {
    pub(crate) area: Option<Rect>,
    pub(crate) rows: Vec<(usize, usize)>,
    anchor: Option<(usize, usize)>,
    pointer: Option<(u16, u16)>,
    last_scroll: Option<Instant>,
}

impl EditorState {
    pub(crate) fn cancel_mouse_drag(&mut self) {
        self.mouse.anchor = None;
        self.mouse.pointer = None;
    }

    pub(crate) fn mouse_drag_active(&self) -> bool {
        self.mouse.anchor.is_some()
    }

    fn rebuild_mouse_rows(&mut self) {
        self.mouse.rows.clear();
        let Some(area) = self.mouse.area else { return };
        for line in self.scroll..self.lines.len() {
            if self.word_wrap {
                let expanded = self.expand_tabs_visual(&self.lines[line]);
                let segments = Self::compute_wrap_segments(&expanded, self.visible_width.max(1));
                let skip = if line == self.scroll {
                    self.wrap_scroll_offset
                } else {
                    0
                };
                for offset in segments.into_iter().skip(skip) {
                    if self.mouse.rows.len() >= area.height as usize {
                        return;
                    }
                    self.mouse.rows.push((line, offset));
                }
            } else {
                if self.mouse.rows.len() >= area.height as usize {
                    return;
                }
                self.mouse.rows.push((line, self.horizontal_scroll));
            }
        }
    }

    fn mouse_document_position(&self, column: u16, row: u16) -> Option<(usize, usize)> {
        let area = self.mouse.area?;
        if area.width == 0 || area.height == 0 || self.lines.is_empty() {
            return None;
        }
        let row_index = row.saturating_sub(area.y).min(area.height - 1) as usize;
        let Some(&(line_index, segment_start)) = self.mouse.rows.get(row_index) else {
            let last = self.lines.len() - 1;
            return Some((last, self.lines[last].chars().count()));
        };
        // A captured drag just beyond the right edge can select the last cell
        // too, even when a wrapped line exactly fills the viewport.
        let target = segment_start + column.saturating_sub(area.x).min(area.width) as usize;
        let line = self.lines.get(line_index)?;
        let mut visual = 0;
        for (index, ch) in line.chars().enumerate() {
            let next = if ch == '\t' {
                (visual / self.tab_size.max(1) + 1) * self.tab_size.max(1)
            } else {
                visual + ch.width().unwrap_or(1)
            };
            // A click on either half of a wide character, or within a tab,
            // belongs to that character rather than the following one.
            if target < next {
                return Some((line_index, index));
            }
            visual = next;
        }
        Some((line_index, line.chars().count()))
    }

    fn extend_mouse_selection(&mut self, column: u16, row: u16) {
        let Some(anchor) = self.mouse.anchor else {
            return;
        };
        let Some(position) = self.mouse_document_position(column, row) else {
            return;
        };
        self.cursor_line = position.0;
        self.cursor_col = position.1;
        let (start, end) = if anchor <= position {
            (anchor, position)
        } else {
            (position, anchor)
        };
        self.selection = (start != end).then_some(Selection {
            start_line: start.0,
            start_col: start.1,
            end_line: end.0,
            end_col: end.1,
        });
        self.find_matching_bracket();
    }

    pub(crate) fn scroll_mouse_rows(&mut self, rows: i32) {
        if self.lines.is_empty() {
            return;
        }
        if self.word_wrap {
            // Find the last full viewport without walking the entire document.
            let mut remaining = self.visible_height.max(1);
            let mut last_top = (0, 0);
            for line in (0..self.lines.len()).rev() {
                let height = self.count_wrapped_rows(line);
                if height >= remaining {
                    last_top = (line, height - remaining);
                    break;
                }
                remaining -= height;
            }
            for _ in 0..rows.unsigned_abs().min(100) {
                if rows > 0 {
                    if (self.scroll, self.wrap_scroll_offset) >= last_top {
                        break;
                    }
                    self.advance_wrap_scroll_top();
                } else if self.wrap_scroll_offset > 0 {
                    self.wrap_scroll_offset -= 1;
                } else if self.scroll > 0 {
                    self.scroll -= 1;
                    self.wrap_scroll_offset =
                        self.count_wrapped_rows(self.scroll).saturating_sub(1);
                }
            }
        } else {
            let max_scroll = self.lines.len().saturating_sub(self.visible_height.max(1));
            self.scroll = crate::ui::mouse::scroll_delta(self.scroll, rows).min(max_scroll);
        }
        self.rebuild_mouse_rows();
    }

    fn max_mouse_horizontal_scroll(&self) -> usize {
        self.mouse
            .rows
            .iter()
            .filter_map(|(line, _)| self.lines.get(*line))
            .map(|line| self.char_to_visual(line, line.chars().count()))
            .max()
            .unwrap_or(0)
            .saturating_sub(self.visible_width.saturating_sub(1))
    }

    pub(crate) fn tick_mouse_drag(&mut self) {
        let (Some(area), Some((column, row)), Some(_)) =
            (self.mouse.area, self.mouse.pointer, self.mouse.anchor)
        else {
            return;
        };
        if self
            .mouse
            .last_scroll
            .is_some_and(|time| time.elapsed() < Duration::from_millis(50))
        {
            return;
        }
        self.mouse.last_scroll = Some(Instant::now());
        let vertical = if row < area.y {
            -1
        } else if row >= area.bottom() {
            1
        } else {
            0
        };
        if vertical != 0 {
            self.scroll_mouse_rows(vertical);
        }
        if !self.word_wrap {
            if column < area.x {
                self.horizontal_scroll = self.horizontal_scroll.saturating_sub(2);
            } else if column >= area.right() {
                self.horizontal_scroll = self.horizontal_scroll.saturating_add(2).min(
                    self.max_mouse_horizontal_scroll()
                        .max(self.horizontal_scroll),
                );
            }
            self.rebuild_mouse_rows();
        }
        self.extend_mouse_selection(column, row);
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) {
        if self.exit_confirm_open || self.goto_mode || self.find_mode != FindReplaceMode::None {
            self.cancel_mouse_drag();
            return;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(position) = self.mouse_document_position(event.column, event.row) else {
                    return;
                };
                let anchor = if event.modifiers.contains(KeyModifiers::SHIFT) {
                    (self.cursor_line, self.cursor_col)
                } else {
                    position
                };
                self.clear_multi_cursor_state();
                self.mouse.anchor = Some(anchor);
                self.mouse.pointer = Some((event.column, event.row));
                self.mouse.last_scroll = None;
                self.extend_mouse_selection(event.column, event.row);
            }
            MouseEventKind::Drag(MouseButton::Left) if self.mouse.anchor.is_some() => {
                self.mouse.pointer = Some((event.column, event.row));
                self.extend_mouse_selection(event.column, event.row);
                self.tick_mouse_drag();
            }
            MouseEventKind::Up(MouseButton::Left) if self.mouse.anchor.is_some() => {
                self.extend_mouse_selection(event.column, event.row);
                self.cancel_mouse_drag();
            }
            MouseEventKind::ScrollUp => self.scroll_mouse_rows(-3),
            MouseEventKind::ScrollDown => self.scroll_mouse_rows(3),
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight if !self.word_wrap => {
                let delta = if event.kind == MouseEventKind::ScrollLeft {
                    -3
                } else {
                    3
                };
                self.horizontal_scroll =
                    crate::ui::mouse::scroll_delta(self.horizontal_scroll, delta).min(
                        self.max_mouse_horizontal_scroll()
                            .max(self.horizontal_scroll),
                    );
                self.rebuild_mouse_rows();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(lines: &[&str], width: u16, height: u16) -> EditorState {
        let mut editor = EditorState::new();
        editor.lines = lines.iter().map(|s| s.to_string()).collect();
        editor.visible_width = width as usize;
        editor.visible_height = height as usize;
        editor.mouse.area = Some(Rect::new(6, 2, width, height));
        editor.rebuild_mouse_rows();
        editor
    }

    fn event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn drag_selects_half_open_ranges_in_both_directions() {
        for (start, end) in [(7, 10), (10, 7)] {
            let mut state = editor(&["abcdef"], 20, 4);
            state.handle_mouse(event(MouseEventKind::Down(MouseButton::Left), start, 2));
            state.handle_mouse(event(MouseEventKind::Drag(MouseButton::Left), end, 2));
            state.handle_mouse(event(MouseEventKind::Up(MouseButton::Left), end, 2));
            assert_eq!(state.get_selected_text(), "bcd");
            assert!(!state.mouse_drag_active());
            assert!(!state.modified);
            assert!(state.undo_stack.is_empty());
        }
    }

    #[test]
    fn mouse_maps_tabs_wide_characters_horizontal_scroll_and_wrap() {
        let mut state = editor(&["가\txy"], 4, 3);
        assert_eq!(state.mouse_document_position(7, 2), Some((0, 0)));
        assert_eq!(state.mouse_document_position(9, 2), Some((0, 1)));
        state.horizontal_scroll = 4;
        state.rebuild_mouse_rows();
        assert_eq!(state.mouse_document_position(6, 2), Some((0, 2)));
        state.horizontal_scroll = 0;
        state.word_wrap = true;
        state.rebuild_mouse_rows();
        assert_eq!(state.mouse_document_position(6, 3), Some((0, 2)));
    }

    #[test]
    fn wheel_preserves_cursor_selection_and_undo_history() {
        let mut state = editor(&["a", "b", "c", "d", "e"], 10, 2);
        state.selection = Some(Selection {
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 1,
        });
        let selection = state.selection;
        state.scroll_mouse_rows(3);
        assert_eq!(state.scroll, 3);
        assert_eq!((state.cursor_line, state.cursor_col), (0, 0));
        assert_eq!(state.selection, selection);
        assert!(state.undo_stack.is_empty());
        state.scroll_mouse_rows(3);
        assert_eq!(state.scroll, 3);
    }

    #[test]
    fn stationary_drag_outside_view_autoscrolls_and_release_stops_it() {
        let mut state = editor(&["one", "two", "three", "four"], 10, 2);
        state.handle_mouse(event(MouseEventKind::Down(MouseButton::Left), 6, 2));
        state.mouse.pointer = Some((8, 4));
        state.tick_mouse_drag();
        assert_eq!(state.scroll, 1);
        assert_eq!(state.cursor_line, 2);
        state.handle_mouse(event(MouseEventKind::Up(MouseButton::Left), 8, 4));
        state.mouse.last_scroll = None;
        state.tick_mouse_drag();
        assert_eq!(state.scroll, 1);
    }

    #[test]
    fn wrapped_drag_selects_across_segments_and_the_final_cell() {
        let mut state = editor(&["abcdefgh"], 4, 2);
        state.word_wrap = true;
        state.rebuild_mouse_rows();
        state.handle_mouse(event(MouseEventKind::Down(MouseButton::Left), 7, 2));
        state.handle_mouse(event(MouseEventKind::Up(MouseButton::Left), 10, 3));
        assert_eq!(state.get_selected_text(), "bcdefgh");
        state.scroll_mouse_rows(3);
        assert_eq!((state.scroll, state.wrap_scroll_offset), (0, 0));
    }

    #[test]
    fn wrapped_wheel_clamps_to_the_last_full_viewport_and_can_return_to_top() {
        let mut state = editor(&["abcdefgh", "ijklmnop"], 4, 3);
        state.word_wrap = true;
        state.rebuild_mouse_rows();
        state.scroll_mouse_rows(3);
        assert_eq!((state.scroll, state.wrap_scroll_offset), (0, 1));
        assert_eq!(state.mouse.rows, vec![(0, 4), (1, 0), (1, 4)]);
        state.scroll_mouse_rows(-3);
        assert_eq!((state.scroll, state.wrap_scroll_offset), (0, 0));
        assert_eq!((state.cursor_line, state.cursor_col), (0, 0));
    }
}
