use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use super::theme::Theme;
use crate::utils::format::pad_to_display_width;

#[derive(Debug, Clone)]
pub struct SearchCriteria {
    pub name: String,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub modified_after: Option<chrono::NaiveDate>,
    pub modified_before: Option<chrono::NaiveDate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchField {
    Name,
    MinSize,
    MaxSize,
    ModifiedAfter,
    ModifiedBefore,
}

impl SearchField {
    pub fn all() -> [SearchField; 5] {
        [
            SearchField::Name,
            SearchField::MinSize,
            SearchField::MaxSize,
            SearchField::ModifiedAfter,
            SearchField::ModifiedBefore,
        ]
    }

    pub fn label(&self) -> &'static str {
        match self {
            SearchField::Name => "Name",
            SearchField::MinSize => "Min Size",
            SearchField::MaxSize => "Max Size",
            SearchField::ModifiedAfter => "After",
            SearchField::ModifiedBefore => "Before",
        }
    }

    pub fn hint(&self) -> &'static str {
        match self {
            SearchField::Name => "Pattern to match",
            SearchField::MinSize => "e.g., 1K, 1M",
            SearchField::MaxSize => "e.g., 1K, 1M",
            SearchField::ModifiedAfter => "YYYY-MM-DD",
            SearchField::ModifiedBefore => "YYYY-MM-DD",
        }
    }
}

#[derive(Default)]
pub struct AdvancedSearchState {
    pub active_field: usize,
    pub values: [String; 5],
    pub active: bool,
}

impl AdvancedSearchState {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.active_field = 0;
        self.values = [
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ];
    }

    pub fn get_criteria(&self) -> SearchCriteria {
        SearchCriteria {
            name: self.values[0].clone(),
            min_size: parse_size(&self.values[1]),
            max_size: parse_size(&self.values[2]),
            modified_after: parse_date(&self.values[3]),
            modified_before: parse_date(&self.values[4]),
        }
    }
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let re = regex::Regex::new(r"^(\d+(?:\.\d+)?)\s*([KMGT]?)$").ok()?;
    let upper = s.to_uppercase();
    let caps = re.captures(&upper)?;

    let num: f64 = caps.get(1)?.as_str().parse().ok()?;
    let unit = caps.get(2).map(|m| m.as_str()).unwrap_or("");

    let multiplier: u64 = match unit {
        "K" => 1024,
        "M" => 1024 * 1024,
        "G" => 1024 * 1024 * 1024,
        "T" => 1024 * 1024 * 1024 * 1024,
        _ => 1,
    };

    Some((num * multiplier as f64) as u64)
}

fn parse_date(s: &str) -> Option<chrono::NaiveDate> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

pub fn draw(
    frame: &mut Frame,
    state: &AdvancedSearchState,
    area: Rect,
    theme: &Theme,
    kb: &crate::keybindings::Keybindings,
) {
    // Clamp to the terminal area so the dialog never exceeds the buffer (Clear/Paragraph
    // render out of bounds and panic otherwise on terminals smaller than 50x12).
    let width = 50u16.min(area.width);
    let height = 12u16.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, dialog_area);

    let block = Block::default()
        .title(" Advanced Search ")
        .title_style(theme.header_style())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.advanced_search.border))
        .border_type(ratatui::widgets::BorderType::Double);

    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let fields = SearchField::all();
    let mut lines = Vec::new();

    for (i, field) in fields.iter().enumerate() {
        let is_active = i == state.active_field;
        let prefix = if is_active { "> " } else { "  " };
        let value = &state.values[i];

        let mut spans = vec![
            Span::styled(
                prefix,
                if is_active {
                    Style::default().fg(theme.advanced_search.border)
                } else {
                    Style::default().fg(theme.advanced_search.label)
                },
            ),
            Span::styled(
                format!("{:10}", field.label()),
                if is_active {
                    Style::default().fg(theme.advanced_search.border)
                } else {
                    Style::default().fg(theme.advanced_search.label)
                },
            ),
            Span::styled(
                "[",
                Style::default().fg(theme.advanced_search.field_bracket),
            ),
            Span::styled(
                pad_to_display_width(value, 12),
                if is_active {
                    theme.selected_style()
                } else {
                    Style::default().fg(theme.advanced_search.input_text)
                },
            ),
            Span::styled(
                "]",
                Style::default().fg(theme.advanced_search.field_bracket),
            ),
        ];

        if is_active {
            spans.push(Span::styled(
                format!(" {}", field.hint()),
                theme.dim_style(),
            ));
        }

        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    {
        use crate::keybindings::AdvancedSearchAction;
        let nav_key = kb.advanced_search_keys_joined(AdvancedSearchAction::MoveDown, "/");
        let submit_key = kb.advanced_search_first_key(AdvancedSearchAction::Submit);
        let cancel_key = kb.advanced_search_first_key(AdvancedSearchAction::Cancel);
        lines.push(Line::from(Span::styled(
            format!(
                "[{}] Navigate  [{}] Search  [{}] Cancel",
                nav_key, submit_key, cancel_key
            ),
            theme.dim_style(),
        )));
    }

    // saturating_sub + intersection: with the dialog clamped to a small terminal, plain
    // `- 2` underflows and the fixed offsets can fall outside the buffer (Paragraph
    // indexes the buffer directly and panics OOB).
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(
            inner.x + 1,
            inner.y + 1,
            inner.width.saturating_sub(2),
            inner.height.saturating_sub(2),
        )
        .intersection(inner),
    );
}

/// Handle paste event for advanced search
pub fn handle_paste(state: &mut AdvancedSearchState, text: &str) {
    // Use only the first line for single-line search fields
    let paste_text = text.lines().next().unwrap_or("").replace('\r', "");
    if !paste_text.is_empty() {
        state.values[state.active_field].push_str(&paste_text);
    }
}

pub fn handle_input(
    state: &mut AdvancedSearchState,
    code: KeyCode,
    modifiers: KeyModifiers,
    kb: &crate::keybindings::Keybindings,
) -> Option<SearchCriteria> {
    use crate::keybindings::AdvancedSearchAction;

    if let Some(action) = kb.advanced_search_action(code, modifiers) {
        match action {
            AdvancedSearchAction::Cancel => {
                state.active = false;
                state.reset();
                return None;
            }
            AdvancedSearchAction::Submit => {
                state.active = false;
                let criteria = state.get_criteria();
                state.reset();
                return Some(criteria);
            }
            AdvancedSearchAction::MoveUp => {
                state.active_field = state.active_field.saturating_sub(1);
                return None;
            }
            AdvancedSearchAction::MoveDown => {
                if state.active_field < 4 {
                    state.active_field += 1;
                }
                return None;
            }
        }
    }

    // Text input (hardcoded)
    match code {
        KeyCode::Backspace => {
            state.values[state.active_field].pop();
        }
        KeyCode::Char(c) => {
            state.values[state.active_field].push(c);
        }
        _ => {}
    }
    None
}

/// Check if a file matches the search criteria
pub fn matches_criteria(
    name: &str,
    size: u64,
    modified: chrono::DateTime<chrono::Local>,
    criteria: &SearchCriteria,
) -> bool {
    // Name match (case-insensitive substring match)
    if !criteria.name.is_empty() {
        let name_lower = name.to_lowercase();
        let pattern_lower = criteria.name.to_lowercase();
        if !name_lower.contains(&pattern_lower) {
            return false;
        }
    }

    // Size range
    if let Some(min) = criteria.min_size {
        if size < min {
            return false;
        }
    }

    if let Some(max) = criteria.max_size {
        if size > max {
            return false;
        }
    }

    // Date range
    let file_date = modified.date_naive();

    if let Some(after) = criteria.modified_after {
        if file_date < after {
            return false;
        }
    }

    if let Some(before) = criteria.modified_before {
        if file_date > before {
            return false;
        }
    }

    true
}
