use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::keybindings::{Keybindings, SearchResultAction};
use crate::ui::theme::Theme;
use crate::utils::format::{display_width_suffix, pad_to_display_width};

const STATUS_WIDTH: usize = 13;
const FULL_HASH_DETAIL_WIDTH: usize = 68;
const HASH_PAIR_DETAIL_WIDTH: usize = 20;
const COMPACT_DETAIL_WIDTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Md5VerificationStatus {
    Match {
        hash: String,
    },
    Mismatch {
        candidates: Vec<String>,
        actual: String,
    },
    NoHash,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Md5VerificationResult {
    pub file_name: String,
    pub status: Md5VerificationStatus,
}

#[derive(Debug, Default)]
pub struct Md5VerificationState {
    pub results: Vec<Md5VerificationResult>,
    pub selected_index: usize,
    pub scroll_offset: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Md5VerificationCounts {
    pub matched: usize,
    pub mismatched: usize,
    pub no_hash: usize,
    pub errors: usize,
}

impl Md5VerificationState {
    pub fn new(results: Vec<Md5VerificationResult>) -> Self {
        Self {
            results,
            selected_index: 0,
            scroll_offset: 0,
        }
    }

    pub fn counts(&self) -> Md5VerificationCounts {
        let mut counts = Md5VerificationCounts::default();
        for result in &self.results {
            match &result.status {
                Md5VerificationStatus::Match { .. } => counts.matched += 1,
                Md5VerificationStatus::Mismatch { .. } => counts.mismatched += 1,
                Md5VerificationStatus::NoHash => counts.no_hash += 1,
                Md5VerificationStatus::Error { .. } => counts.errors += 1,
            }
        }
        counts
    }

    pub fn move_cursor(&mut self, delta: i32) {
        if self.results.is_empty() {
            return;
        }
        self.selected_index = (self.selected_index as i32 + delta)
            .max(0)
            .min(self.results.len().saturating_sub(1) as i32)
            as usize;
    }

    pub fn cursor_to_start(&mut self) {
        self.selected_index = 0;
    }

    pub fn cursor_to_end(&mut self) {
        if !self.results.is_empty() {
            self.selected_index = self.results.len() - 1;
        }
    }

    fn adjust_scroll(&mut self, visible_height: usize) {
        if visible_height == 0 {
            return;
        }
        if self.selected_index < self.scroll_offset {
            self.scroll_offset = self.selected_index;
        } else if self.selected_index >= self.scroll_offset + visible_height {
            self.scroll_offset = self.selected_index - visible_height + 1;
        }
    }
}

fn single_line_text(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn suffix_cell(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let text = single_line_text(text);
    if text.width() <= width {
        return pad_to_display_width(&text, width);
    }
    if width <= 3 {
        return pad_to_display_width(&display_width_suffix(&text, width), width);
    }
    let suffix = display_width_suffix(&text, width - 3);
    pad_to_display_width(&format!("...{}", suffix), width)
}

fn hash_prefix(hash: &str) -> &str {
    hash.get(..8).unwrap_or(hash)
}

fn status_presentation(
    status: &Md5VerificationStatus,
    detail_width: usize,
) -> (&'static str, &'static str, String) {
    match status {
        Md5VerificationStatus::Match { hash } => {
            let detail = if detail_width == 0 {
                String::new()
            } else if detail_width >= 32 {
                hash.clone()
            } else {
                hash_prefix(hash).to_string()
            };
            ("✓", "MATCH", detail)
        }
        Md5VerificationStatus::Mismatch { candidates, actual } => {
            let detail = match candidates.as_slice() {
                [expected] if detail_width >= FULL_HASH_DETAIL_WIDTH => {
                    format!("{} != {}", expected, actual)
                }
                [expected] if detail_width >= HASH_PAIR_DETAIL_WIDTH => {
                    format!("{} != {}", hash_prefix(expected), hash_prefix(actual))
                }
                _ if detail_width >= FULL_HASH_DETAIL_WIDTH => {
                    format!("none of {} candidates; actual {}", candidates.len(), actual)
                }
                _ if detail_width >= HASH_PAIR_DETAIL_WIDTH => {
                    format!("{} hashes != {}", candidates.len(), hash_prefix(actual))
                }
                _ if detail_width >= COMPACT_DETAIL_WIDTH => hash_prefix(actual).to_string(),
                _ => String::new(),
            };
            ("✕", "MISMATCH", detail)
        }
        Md5VerificationStatus::NoHash => ("–", "NO HASH", String::new()),
        Md5VerificationStatus::Error { message } => ("!", "ERROR", single_line_text(message)),
    }
}

fn status_style(theme: &Theme, status: &Md5VerificationStatus) -> Style {
    match status {
        Md5VerificationStatus::Match { .. } => theme.success_style(),
        Md5VerificationStatus::Mismatch { .. } | Md5VerificationStatus::Error { .. } => {
            theme.error_style().add_modifier(Modifier::BOLD)
        }
        Md5VerificationStatus::NoHash => theme.warning_style(),
    }
}

pub fn draw(
    frame: &mut Frame,
    state: &mut Md5VerificationState,
    area: Rect,
    theme: &Theme,
    keybindings: &Keybindings,
) {
    let block = Block::default()
        .title(" MD5 Verification ")
        .title_style(theme.header_style())
        .borders(Borders::ALL)
        .border_style(theme.border_style(true));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let counts = state.counts();
    let summary = Line::from(vec![
        Span::styled(
            format!("{} files", state.results.len()),
            theme.header_style(),
        ),
        Span::styled("  ", theme.dim_style()),
        Span::styled(format!("{} match", counts.matched), theme.success_style()),
        Span::styled("  ", theme.dim_style()),
        Span::styled(
            format!("{} mismatch", counts.mismatched),
            theme.error_style(),
        ),
        Span::styled("  ", theme.dim_style()),
        Span::styled(format!("{} no hash", counts.no_hash), theme.warning_style()),
        Span::styled("  ", theme.dim_style()),
        Span::styled(format!("{} error", counts.errors), theme.error_style()),
    ]);
    frame.render_widget(
        Paragraph::new(summary),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    if inner.height < 3 {
        return;
    }

    let row_width = inner.width.saturating_sub(1) as usize;
    let status_width = STATUS_WIDTH.min(row_width);
    let detail_width = if row_width >= 105 {
        FULL_HASH_DETAIL_WIDTH
    } else if row_width >= 62 {
        HASH_PAIR_DETAIL_WIDTH
    } else if row_width >= 48 {
        COMPACT_DETAIL_WIDTH
    } else {
        0
    };
    let name_width = row_width.saturating_sub(status_width + detail_width);

    let header = Line::from(vec![
        Span::styled(
            pad_to_display_width("  STATUS", status_width),
            theme.dim_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            pad_to_display_width("FILE", name_width),
            theme.dim_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            pad_to_display_width("HASH", detail_width),
            theme.dim_style().add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );

    let footer_y = inner.y + inner.height - 1;
    let list_area = Rect::new(
        inner.x,
        inner.y + 2,
        inner.width,
        inner.height.saturating_sub(3),
    );
    let visible_height = list_area.height as usize;
    state.adjust_scroll(visible_height);

    let mut lines = Vec::new();
    for (visible_index, result) in state
        .results
        .iter()
        .skip(state.scroll_offset)
        .take(visible_height)
        .enumerate()
    {
        let result_index = state.scroll_offset + visible_index;
        let selected = result_index == state.selected_index;
        let (symbol, label, detail) = status_presentation(&result.status, detail_width);
        let row_style = if selected {
            theme.selected_style()
        } else {
            theme.normal_style()
        };
        let semantic_style = if selected {
            row_style
        } else {
            status_style(theme, &result.status)
        };
        let status_text = format!("{}{} {}", if selected { "> " } else { "  " }, symbol, label);
        lines.push(Line::from(vec![
            Span::styled(
                pad_to_display_width(&status_text, status_width),
                semantic_style,
            ),
            Span::styled(suffix_cell(&result.file_name, name_width), row_style),
            Span::styled(suffix_cell(&detail, detail_width), row_style),
        ]));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No verification results",
            theme.dim_style(),
        )));
    }
    frame.render_widget(Paragraph::new(lines), list_area);

    if state.results.len() > visible_height && visible_height > 0 {
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));
        let mut scrollbar_state =
            ScrollbarState::new(state.results.len()).position(state.selected_index);
        let scrollbar_area = Rect::new(inner.x + inner.width - 1, list_area.y, 1, list_area.height);
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }

    let navigation = format!(
        "{}/{}",
        keybindings.search_result_first_key(SearchResultAction::MoveUp),
        keybindings.search_result_first_key(SearchResultAction::MoveDown)
    );
    let paging = format!(
        "{}/{}",
        keybindings.search_result_first_key(SearchResultAction::PageUp),
        keybindings.search_result_first_key(SearchResultAction::PageDown)
    );
    let footer = Line::from(vec![
        Span::styled(navigation, theme.header_style()),
        Span::styled(":navigate  ", theme.dim_style()),
        Span::styled(paging, theme.header_style()),
        Span::styled(":page  ", theme.dim_style()),
        Span::styled(
            keybindings
                .search_result_first_key(SearchResultAction::Close)
                .to_string(),
            theme.header_style(),
        ),
        Span::styled(":close", theme.dim_style()),
    ]);
    frame.render_widget(
        Paragraph::new(footer),
        Rect::new(inner.x, footer_y, inner.width, 1),
    );
}

pub fn handle_input(
    state: &mut Md5VerificationState,
    code: KeyCode,
    modifiers: KeyModifiers,
    keybindings: &Keybindings,
) -> bool {
    let Some(action) = keybindings.search_result_action(code, modifiers) else {
        return false;
    };
    match action {
        SearchResultAction::Close => return true,
        SearchResultAction::MoveUp => state.move_cursor(-1),
        SearchResultAction::MoveDown => state.move_cursor(1),
        SearchResultAction::PageUp => state.move_cursor(-10),
        SearchResultAction::PageDown => state.move_cursor(10),
        SearchResultAction::GoHome => state.cursor_to_start(),
        SearchResultAction::GoEnd => state.cursor_to_end(),
        SearchResultAction::Open => {}
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::KeybindingsConfig;
    use ratatui::{backend::TestBackend, Terminal};

    fn rendered_row(terminal: &Terminal<TestBackend>, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| terminal.backend().buffer().cell((x, y)).unwrap().symbol())
            .collect()
    }

    #[test]
    fn counts_each_result_status() {
        let state = Md5VerificationState::new(vec![
            Md5VerificationResult {
                file_name: "match".to_string(),
                status: Md5VerificationStatus::Match {
                    hash: "a".repeat(32),
                },
            },
            Md5VerificationResult {
                file_name: "mismatch".to_string(),
                status: Md5VerificationStatus::Mismatch {
                    candidates: vec!["a".repeat(32)],
                    actual: "b".repeat(32),
                },
            },
            Md5VerificationResult {
                file_name: "missing".to_string(),
                status: Md5VerificationStatus::NoHash,
            },
            Md5VerificationResult {
                file_name: "error".to_string(),
                status: Md5VerificationStatus::Error {
                    message: "unreadable".to_string(),
                },
            },
        ]);

        assert_eq!(
            state.counts(),
            Md5VerificationCounts {
                matched: 1,
                mismatched: 1,
                no_hash: 1,
                errors: 1,
            }
        );
    }

    #[test]
    fn multiple_candidates_are_presented_as_one_mismatch() {
        let actual = "c".repeat(32);
        let status = Md5VerificationStatus::Mismatch {
            candidates: vec!["a".repeat(32), "b".repeat(32)],
            actual: actual.clone(),
        };

        let (_, label, detail) = status_presentation(&status, FULL_HASH_DETAIL_WIDTH);

        assert_eq!(label, "MISMATCH");
        assert!(detail.contains("2 candidates"));
        assert!(detail.contains(&actual));
    }

    #[test]
    fn row_cells_never_contain_control_characters() {
        let cell = suffix_cell("bad\nname\t.txt", 20);
        assert!(!cell.chars().any(char::is_control));
        assert_eq!(cell.width(), 20);
    }

    #[test]
    fn cursor_navigation_is_bounded() {
        let results = (0..3)
            .map(|index| Md5VerificationResult {
                file_name: index.to_string(),
                status: Md5VerificationStatus::NoHash,
            })
            .collect();
        let mut state = Md5VerificationState::new(results);

        state.move_cursor(-10);
        assert_eq!(state.selected_index, 0);
        state.move_cursor(10);
        assert_eq!(state.selected_index, 2);
        state.cursor_to_start();
        assert_eq!(state.selected_index, 0);
        state.cursor_to_end();
        assert_eq!(state.selected_index, 2);
    }

    #[test]
    fn eighty_column_render_keeps_each_file_on_one_row() {
        let mut state = Md5VerificationState::new(vec![
            Md5VerificationResult {
                file_name: "hello.5d41402abc4b2a76b9719d911017c592.tar".to_string(),
                status: Md5VerificationStatus::Match {
                    hash: "5d41402abc4b2a76b9719d911017c592".to_string(),
                },
            },
            Md5VerificationResult {
                file_name: "apple.5d41402abc4b2a76b9719d911017c592.tar".to_string(),
                status: Md5VerificationStatus::Mismatch {
                    candidates: vec!["5d41402abc4b2a76b9719d911017c592".to_string()],
                    actual: "8977dfac2f8e04cb96e66882235f5aba".to_string(),
                },
            },
        ]);
        let theme = Theme::default();
        let keybindings = Keybindings::from_config(&KeybindingsConfig::default());
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut state,
                    Rect::new(0, 0, 80, 24),
                    &theme,
                    &keybindings,
                );
            })
            .unwrap();

        let first = rendered_row(&terminal, 3, 80);
        let second = rendered_row(&terminal, 4, 80);
        assert!(first.contains("MATCH"));
        assert!(!first.contains("MISMATCH"));
        assert!(second.contains("MISMATCH"));
    }
}
