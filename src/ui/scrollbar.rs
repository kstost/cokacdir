use ratatui::widgets::ScrollbarState;

/// Build a scrollbar for a viewport offset, measured in rendered rows.
pub(crate) fn viewport_state(
    total_rows: usize,
    visible_rows: usize,
    offset: usize,
) -> Option<ScrollbarState> {
    if visible_rows == 0 || total_rows <= visible_rows {
        return None;
    }

    let max_scroll = total_rows.saturating_sub(visible_rows);
    // Ratatui treats content_length - 1 as the last possible position.
    Some(
        ScrollbarState::new(max_scroll + 1)
            .position(offset.min(max_scroll))
            .viewport_content_length(visible_rows),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        widgets::{Scrollbar, ScrollbarOrientation, StatefulWidget},
    };

    fn render_track(total: usize, visible: u16, offset: usize) -> Vec<String> {
        let area = Rect::new(0, 0, 1, visible);
        let mut buffer = Buffer::empty(area);
        let mut state = viewport_state(total, visible as usize, offset).unwrap();
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .thumb_symbol("█")
            .track_symbol(Some("░"))
            .render(area, &mut buffer, &mut state);
        (0..visible)
            .map(|y| buffer[(0, y)].symbol().to_string())
            .collect()
    }

    #[test]
    fn one_row_overflow_moves_the_thumb_to_both_ends() {
        assert_eq!(render_track(7, 6, 0), ["█", "█", "█", "█", "█", "░"]);
        assert_eq!(render_track(7, 6, 1), ["░", "█", "█", "█", "█", "█"]);
    }

    #[test]
    fn viewport_bottom_and_overscroll_render_at_the_end_of_the_track() {
        let top = render_track(100, 20, 0);
        let bottom = render_track(100, 20, 80);
        assert_eq!((&top[0][..], &top[19][..]), ("█", "░"));
        assert_eq!((&bottom[0][..], &bottom[19][..]), ("░", "█"));
        assert_eq!(render_track(100, 20, usize::MAX), bottom);
        assert_eq!(render_track(100, 40, 80), render_track(100, 40, 60));
    }

    #[test]
    fn empty_hidden_and_fully_visible_content_need_no_scrollbar() {
        for (total, visible) in [(0, 0), (0, 10), (100, 0), (9, 10), (10, 10)] {
            assert!(viewport_state(total, visible, usize::MAX).is_none());
        }
    }
}
