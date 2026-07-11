use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

/// Check if a Line is effectively empty (all spans contain only whitespace)
/// Handles both ASCII and Unicode whitespace characters
pub fn is_line_empty(line: &Line) -> bool {
    if line.spans.is_empty() {
        return true;
    }
    line.spans
        .iter()
        .all(|span| span.content.chars().all(|c| c.is_whitespace()))
}

/// Parse Markdown text and return styled lines for ratatui
pub fn render_markdown(text: &str, theme_colors: MarkdownTheme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code_block = false;
    let mut _code_block_lang: Option<String> = None;
    let mut code_block_lines: Vec<String> = Vec::new();

    let text_lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < text_lines.len() {
        let line = text_lines[i];

        // Handle code block
        if line.trim().starts_with("```") {
            if in_code_block {
                // End code block
                for code_line in &code_block_lines {
                    lines.push(Line::from(vec![
                        Span::styled("  ", Style::default()),
                        Span::styled(code_line.clone(), Style::default().fg(theme_colors.code)),
                    ]));
                }
                code_block_lines.clear();
                in_code_block = false;
                _code_block_lang = None;
            } else {
                // Start code block
                in_code_block = true;
                let lang = line.trim().trim_start_matches("```").trim();
                if !lang.is_empty() {
                    _code_block_lang = Some(lang.to_string());
                    lines.push(Line::from(Span::styled(
                        format!("  [{}]", lang),
                        Style::default().fg(theme_colors.dim),
                    )));
                }
            }
            i += 1;
            continue;
        }

        if in_code_block {
            code_block_lines.push(line.to_string());
            i += 1;
            continue;
        }

        // Handle table (lines starting with |)
        if line.trim().starts_with('|') && line.trim().ends_with('|') {
            // Collect all table lines
            let mut table_lines: Vec<&str> = vec![line];
            let mut j = i + 1;
            while j < text_lines.len() {
                let next_line = text_lines[j];
                if next_line.trim().starts_with('|') && next_line.trim().ends_with('|') {
                    table_lines.push(next_line);
                    j += 1;
                } else {
                    break;
                }
            }

            // Parse and render table
            let table_rendered = render_table(&table_lines, &theme_colors);
            lines.extend(table_rendered);

            i = j;
            continue;
        }

        // Handle headers
        if line.starts_with("#### ") {
            let content = line.trim_start_matches("#### ");
            lines.push(Line::from(Span::styled(
                format!("    {}", content),
                Style::default()
                    .fg(theme_colors.dim)
                    .add_modifier(Modifier::ITALIC),
            )));
            i += 1;
            continue;
        }
        if line.starts_with("### ") {
            let content = line.trim_start_matches("### ");
            lines.push(Line::from(Span::styled(
                format!("   {}", content),
                Style::default()
                    .fg(theme_colors.text)
                    .add_modifier(Modifier::BOLD),
            )));
            i += 1;
            continue;
        }
        if line.starts_with("## ") {
            let content = line.trim_start_matches("## ");
            lines.push(Line::from(Span::styled(
                format!("  {}", content),
                Style::default()
                    .fg(theme_colors.header)
                    .add_modifier(Modifier::BOLD),
            )));
            i += 1;
            continue;
        }
        if line.starts_with("# ") {
            let content = line.trim_start_matches("# ");
            lines.push(Line::from(Span::styled(
                format!(" {}", content),
                Style::default()
                    .fg(theme_colors.header)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )));
            i += 1;
            continue;
        }

        // Handle horizontal rule
        if line
            .trim()
            .chars()
            .all(|c| c == '-' || c == '*' || c == '_')
            && line.trim().len() >= 3
        {
            lines.push(Line::from(Span::styled(
                "─".repeat(40),
                Style::default().fg(theme_colors.dim),
            )));
            i += 1;
            continue;
        }

        // Handle blockquote (> text or >> nested)
        if line.starts_with('>') {
            let mut depth = 0;
            let mut remaining = line;
            while remaining.starts_with('>') {
                depth += 1;
                remaining = remaining[1..].trim_start();
            }
            let indent = "│ ".repeat(depth);
            let spans = parse_inline_markdown(remaining, &theme_colors);
            let mut result = vec![Span::styled(
                indent,
                Style::default().fg(theme_colors.blockquote),
            )];
            result.extend(spans.into_iter().map(|mut s| {
                s.style = s.style.add_modifier(Modifier::ITALIC);
                s
            }));
            lines.push(Line::from(result));
            i += 1;
            continue;
        }

        // Handle checkbox list (- [ ] or - [x])
        if let Some(rest) = line
            .strip_prefix("- [ ] ")
            .or_else(|| line.strip_prefix("* [ ] "))
        {
            let spans = parse_inline_markdown(rest, &theme_colors);
            let mut result = vec![
                Span::styled("  ", Style::default()),
                Span::styled("☐ ", Style::default().fg(theme_colors.dim)),
            ];
            result.extend(spans);
            lines.push(Line::from(result));
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("- [x] ").or_else(|| {
            line.strip_prefix("* [x] ").or_else(|| {
                line.strip_prefix("- [X] ")
                    .or_else(|| line.strip_prefix("* [X] "))
            })
        }) {
            let spans = parse_inline_markdown(rest, &theme_colors);
            let mut result = vec![
                Span::styled("  ", Style::default()),
                Span::styled("☑ ", Style::default().fg(theme_colors.success)),
            ];
            result.extend(spans.into_iter().map(|mut s| {
                s.style = s.style.add_modifier(Modifier::CROSSED_OUT);
                s
            }));
            lines.push(Line::from(result));
            i += 1;
            continue;
        }

        // Handle nested unordered list (with indentation)
        if let Some((indent_level, content)) = parse_nested_list(line, &['-', '*', '+']) {
            let indent = "  ".repeat(indent_level);
            let bullet = if indent_level == 0 {
                "• "
            } else if indent_level == 1 {
                "◦ "
            } else {
                "▪ "
            };
            let spans = parse_inline_markdown(content, &theme_colors);
            let mut result = vec![Span::styled(
                format!("{}{}", indent, bullet),
                Style::default().fg(theme_colors.text),
            )];
            result.extend(spans);
            lines.push(Line::from(result));
            i += 1;
            continue;
        }

        // Handle unordered list
        if let Some(content) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* ").or_else(|| line.strip_prefix("+ ")))
        {
            let spans = parse_inline_markdown(content, &theme_colors);
            let mut result = vec![Span::styled("  • ", Style::default().fg(theme_colors.text))];
            result.extend(spans);
            lines.push(Line::from(result));
            i += 1;
            continue;
        }

        // Handle ordered list
        if let Some(pos) = line.find(". ") {
            let prefix = &line[..pos];
            if prefix.chars().all(|c| c.is_ascii_digit()) {
                let content = &line[pos + 2..];
                let spans = parse_inline_markdown(content, &theme_colors);
                let mut result = vec![Span::styled(
                    format!("  {}. ", prefix),
                    Style::default().fg(theme_colors.text),
                )];
                result.extend(spans);
                lines.push(Line::from(result));
                i += 1;
                continue;
            }
        }

        // Handle empty line
        if line.trim().is_empty() {
            lines.push(Line::from(""));
            i += 1;
            continue;
        }

        // Regular text with inline formatting
        let spans = parse_inline_markdown(line, &theme_colors);
        lines.push(Line::from(spans));
        i += 1;
    }

    // Handle unclosed code block
    if in_code_block {
        for code_line in &code_block_lines {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(code_line.clone(), Style::default().fg(theme_colors.code)),
            ]));
        }
    }

    // Remove consecutive empty lines (keep at most one)
    let mut result: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    let mut prev_was_empty = false;

    for line in lines {
        if is_line_empty(&line) {
            if !prev_was_empty {
                result.push(line);
            }
            prev_was_empty = true;
        } else {
            result.push(line);
            prev_was_empty = false;
        }
    }

    result
}

/// Render a markdown table
fn render_table(table_lines: &[&str], theme: &MarkdownTheme) -> Vec<Line<'static>> {
    let mut result: Vec<Line<'static>> = Vec::new();

    if table_lines.is_empty() {
        return result;
    }

    // Parse cells from each row
    let rows: Vec<Vec<String>> = table_lines
        .iter()
        .map(|line| {
            line.trim()
                .trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().to_string())
                .collect()
        })
        .collect();

    if rows.is_empty() {
        return result;
    }

    // Find separator row (contains only -, :, |, space)
    let separator_idx = rows.iter().position(|row| {
        row.iter().all(|cell| {
            cell.chars().all(|c| c == '-' || c == ':' || c == ' ') && cell.contains('-')
        })
    });

    // Calculate column widths using unicode width (for CJK characters)
    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut col_widths: Vec<usize> = vec![0; num_cols];
    for row in &rows {
        for (col_idx, cell) in row.iter().enumerate() {
            if col_idx < num_cols {
                // Skip separator row for width calculation
                if !cell.chars().all(|c| c == '-' || c == ':' || c == ' ') {
                    col_widths[col_idx] =
                        col_widths[col_idx].max(UnicodeWidthStr::width(cell.as_str()));
                }
            }
        }
    }

    // Render top border
    let top_border: String = col_widths
        .iter()
        .map(|w| "─".repeat(w + 2))
        .collect::<Vec<_>>()
        .join("┬");
    result.push(Line::from(Span::styled(
        format!("┌{}┐", top_border),
        Style::default().fg(theme.dim),
    )));

    // Render rows
    for (row_idx, row) in rows.iter().enumerate() {
        // Skip separator row
        if Some(row_idx) == separator_idx {
            // Render middle border
            let mid_border: String = col_widths
                .iter()
                .map(|w| "─".repeat(w + 2))
                .collect::<Vec<_>>()
                .join("┼");
            result.push(Line::from(Span::styled(
                format!("├{}┤", mid_border),
                Style::default().fg(theme.dim),
            )));
            continue;
        }

        // Render data row
        let mut spans: Vec<Span<'static>> = vec![Span::styled("│", Style::default().fg(theme.dim))];

        for (col_idx, width) in col_widths.iter().enumerate() {
            let cell_content = row.get(col_idx).map(|s| s.as_str()).unwrap_or("");
            // Calculate padding using unicode width
            let cell_width = UnicodeWidthStr::width(cell_content);
            let padding = width.saturating_sub(cell_width);
            let padded = format!(" {}{} ", cell_content, " ".repeat(padding));

            // Header row (before separator) gets bold style
            let is_header = separator_idx.map(|idx| row_idx < idx).unwrap_or(false);
            let style = if is_header {
                Style::default()
                    .fg(theme.header)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };

            spans.push(Span::styled(padded, style));
            spans.push(Span::styled("│", Style::default().fg(theme.dim)));
        }

        result.push(Line::from(spans));
    }

    // Render bottom border
    let bottom_border: String = col_widths
        .iter()
        .map(|w| "─".repeat(w + 2))
        .collect::<Vec<_>>()
        .join("┴");
    result.push(Line::from(Span::styled(
        format!("└{}┘", bottom_border),
        Style::default().fg(theme.dim),
    )));

    result
}

/// Parse inline Markdown (bold, italic, code, links)
fn parse_inline_markdown(text: &str, theme: &MarkdownTheme) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current_pos = 0;
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    // If a malformed opening bracket has already scanned through a closing
    // bracket (or the rest of the line), do not rescan that same suffix for
    // every nested `[`. This keeps malformed/untrusted Markdown linear.
    let mut next_link_attempt = 0usize;

    while current_pos < len {
        // Check for bold+italic (***text***)
        if current_pos + 2 < len
            && chars[current_pos] == '*'
            && chars[current_pos + 1] == '*'
            && chars[current_pos + 2] == '*'
        {
            if let Some(end) = find_closing_marker(&chars, current_pos + 3, "***") {
                let content: String = chars[current_pos + 3..end].iter().collect();
                spans.push(Span::styled(
                    content,
                    Style::default()
                        .fg(theme.text)
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                ));
                current_pos = end + 3;
                continue;
            }
        }

        // Check for bold (**text**)
        if current_pos + 1 < len && chars[current_pos] == '*' && chars[current_pos + 1] == '*' {
            if let Some(end) = find_closing_marker(&chars, current_pos + 2, "**") {
                let content: String = chars[current_pos + 2..end].iter().collect();
                spans.push(Span::styled(
                    content,
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ));
                current_pos = end + 2;
                continue;
            }
        }

        // Check for inline code (`code`)
        if chars[current_pos] == '`' {
            if let Some(end) = find_closing_char(&chars, current_pos + 1, '`') {
                let content: String = chars[current_pos + 1..end].iter().collect();
                spans.push(Span::styled(content, Style::default().fg(theme.code)));
                current_pos = end + 1;
                continue;
            }
        }

        // Check for italic (*text* or _text_)
        if chars[current_pos] == '*' || chars[current_pos] == '_' {
            let marker = chars[current_pos];
            if let Some(end) = find_closing_char(&chars, current_pos + 1, marker) {
                // Make sure it's not part of a word (for underscores)
                let content: String = chars[current_pos + 1..end].iter().collect();
                spans.push(Span::styled(
                    content,
                    Style::default()
                        .fg(theme.text)
                        .add_modifier(Modifier::ITALIC),
                ));
                current_pos = end + 1;
                continue;
            }
        }

        // Check for strikethrough (~~text~~)
        if current_pos + 1 < len && chars[current_pos] == '~' && chars[current_pos + 1] == '~' {
            if let Some(end) = find_closing_marker(&chars, current_pos + 2, "~~") {
                let content: String = chars[current_pos + 2..end].iter().collect();
                spans.push(Span::styled(
                    content,
                    Style::default()
                        .fg(theme.dim)
                        .add_modifier(Modifier::CROSSED_OUT),
                ));
                current_pos = end + 2;
                continue;
            }
        }

        // Check for link [text](url)
        if chars[current_pos] == '[' && current_pos >= next_link_attempt {
            match parse_link(&chars, current_pos) {
                LinkParseResult::Match(link_text, url, end_pos) => {
                    spans.push(Span::styled(
                        link_text,
                        Style::default()
                            .fg(theme.link)
                            .add_modifier(Modifier::UNDERLINED),
                    ));
                    spans.push(Span::styled(
                        format!(" ({})", url),
                        Style::default().fg(theme.dim),
                    ));
                    current_pos = end_pos;
                    continue;
                }
                LinkParseResult::RetryAfter(position) => {
                    next_link_attempt = position;
                }
                LinkParseResult::NoLaterMatch => {
                    next_link_attempt = len;
                }
            }
        }

        // Regular character
        spans.push(Span::styled(
            chars[current_pos].to_string(),
            Style::default().fg(theme.text),
        ));
        current_pos += 1;
    }

    // Merge consecutive spans with same style
    merge_spans(spans)
}

fn find_closing_char(chars: &[char], start: usize, marker: char) -> Option<usize> {
    for i in start..chars.len() {
        if chars[i] == marker {
            return Some(i);
        }
    }
    None
}

fn find_closing_marker(chars: &[char], start: usize, marker: &str) -> Option<usize> {
    let marker_chars: Vec<char> = marker.chars().collect();
    let marker_len = marker_chars.len();

    for i in start..=chars.len().saturating_sub(marker_len) {
        let mut matches = true;
        for (j, mc) in marker_chars.iter().enumerate() {
            if chars.get(i + j) != Some(mc) {
                matches = false;
                break;
            }
        }
        if matches {
            return Some(i);
        }
    }
    None
}

enum LinkParseResult {
    Match(String, String, usize),
    RetryAfter(usize),
    NoLaterMatch,
}

fn parse_link(chars: &[char], start: usize) -> LinkParseResult {
    // Find closing bracket
    let mut bracket_end = None;
    for i in start + 1..chars.len() {
        if chars[i] == ']' {
            bracket_end = Some(i);
            break;
        }
    }
    let Some(bracket_end) = bracket_end else {
        return LinkParseResult::NoLaterMatch;
    };

    // Check for opening parenthesis
    if bracket_end + 1 >= chars.len() || chars[bracket_end + 1] != '(' {
        // Every `[` before this first `]` would choose the same invalid closing
        // bracket under the parser's existing semantics.
        return LinkParseResult::RetryAfter(bracket_end + 1);
    }

    // Find closing parenthesis
    let mut paren_end = None;
    for i in bracket_end + 2..chars.len() {
        if chars[i] == ')' {
            paren_end = Some(i);
            break;
        }
    }
    let Some(paren_end) = paren_end else {
        return LinkParseResult::NoLaterMatch;
    };

    let link_text: String = chars[start + 1..bracket_end].iter().collect();
    let url: String = chars[bracket_end + 2..paren_end].iter().collect();

    LinkParseResult::Match(link_text, url, paren_end + 1)
}

/// Parse nested list item and return (indent_level, content)
fn parse_nested_list<'a>(line: &'a str, markers: &[char]) -> Option<(usize, &'a str)> {
    let mut indent = 0;
    let mut chars = line.chars().peekable();

    // Count leading spaces (2 spaces = 1 indent level)
    while chars.peek() == Some(&' ') {
        chars.next();
        indent += 1;
    }

    let indent_level = indent / 2;

    // Only consider it a nested list if there's actual indentation
    if indent_level == 0 {
        return None;
    }

    // Check for list marker
    let rest = &line[indent..];
    for marker in markers {
        let prefix = format!("{} ", marker);
        if let Some(content) = rest.strip_prefix(&prefix) {
            return Some((indent_level, content));
        }
    }

    None
}

fn merge_spans(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    if spans.is_empty() {
        return spans;
    }

    let mut result: Vec<Span<'static>> = Vec::new();
    let mut current_content = String::new();
    let mut current_style = spans[0].style;

    for span in spans {
        if span.style == current_style {
            current_content.push_str(&span.content);
        } else {
            if !current_content.is_empty() {
                result.push(Span::styled(current_content, current_style));
            }
            current_content = span.content.to_string();
            current_style = span.style;
        }
    }

    if !current_content.is_empty() {
        result.push(Span::styled(current_content, current_style));
    }

    result
}

/// Theme colors for Markdown rendering
#[derive(Clone, Copy)]
pub struct MarkdownTheme {
    pub text: Color,
    pub dim: Color,
    pub header: Color,
    pub code: Color,
    pub link: Color,
    pub blockquote: Color,
    pub success: Color,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        Self {
            text: Color::White,
            dim: Color::Gray,
            header: Color::Cyan,
            code: Color::Yellow,
            link: Color::Cyan,
            blockquote: Color::Magenta,
            success: Color::Green,
        }
    }
}

impl MarkdownTheme {
    /// Create MarkdownTheme using only Panel/Viewer/Editor palette colors
    pub fn from_theme(theme: &crate::ui::theme::Theme) -> Self {
        Self {
            text: theme.palette.fg,              // 기본 텍스트
            dim: theme.palette.fg_dim,           // 흐린 텍스트
            header: theme.panel.directory_text,  // 디렉토리 색상
            code: theme.editor.footer_key,       // 코드 색상
            link: theme.panel.directory_text,    // 링크 색상
            blockquote: theme.panel.header_text, // 인용 색상
            success: theme.editor.footer_key,    // 성공 색상
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_header() {
        let theme = MarkdownTheme::default();
        let lines = render_markdown("# Header", theme);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn test_render_code_block() {
        let theme = MarkdownTheme::default();
        let text = "```rust\nlet x = 1;\n```";
        let lines = render_markdown(text, theme);
        assert!(lines.len() >= 2);
    }

    #[test]
    fn test_render_list() {
        let theme = MarkdownTheme::default();
        let lines = render_markdown("- Item 1\n- Item 2", theme);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn malformed_nested_link_markers_render_without_repeated_suffix_scans() {
        let input = "[".repeat(20_000);
        let spans = parse_inline_markdown(&input, &MarkdownTheme::default());
        let rendered: String = spans.iter().map(|span| span.content.as_ref()).collect();

        assert_eq!(rendered, input);
    }

    #[test]
    fn test_render_table() {
        let theme = MarkdownTheme::default();
        let text = "| A | B |\n|---|---|\n| 1 | 2 |";
        let lines = render_markdown(text, theme);
        // Should have: top border, header row, separator, data row, bottom border
        assert!(lines.len() >= 4);
    }

    #[test]
    fn test_render_blockquote() {
        let theme = MarkdownTheme::default();
        let lines = render_markdown("> Quote text", theme);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn test_render_checkbox() {
        let theme = MarkdownTheme::default();
        let lines = render_markdown("- [ ] Todo\n- [x] Done", theme);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_is_line_empty() {
        // Empty spans
        assert!(is_line_empty(&Line::from("")));

        // Single whitespace span
        assert!(is_line_empty(&Line::from("   ")));

        // Multiple whitespace spans
        let multi_span = Line::from(vec![Span::raw("  "), Span::raw("   ")]);
        assert!(is_line_empty(&multi_span));

        // Non-empty line
        assert!(!is_line_empty(&Line::from("text")));

        // Mix of whitespace and content
        let mixed = Line::from(vec![Span::raw("  "), Span::raw("text")]);
        assert!(!is_line_empty(&mixed));

        // Unicode whitespace (non-breaking space, tab, etc.)
        assert!(is_line_empty(&Line::from("\t\t")));
        assert!(is_line_empty(&Line::from("\u{00A0}"))); // Non-breaking space
    }

    #[test]
    fn test_consecutive_empty_lines_removed() {
        let theme = MarkdownTheme::default();
        // Three consecutive empty lines should be reduced to one
        let text = "Line 1\n\n\n\nLine 2";
        let lines = render_markdown(text, theme);

        // Count empty lines
        let empty_count = lines.iter().filter(|line| is_line_empty(line)).count();

        // Should have at most 1 empty line between content
        assert!(
            empty_count <= 1,
            "Expected at most 1 empty line, got {}",
            empty_count
        );
    }

    #[test]
    fn test_no_consecutive_empty_lines() {
        let theme = MarkdownTheme::default();

        // Test various inputs with consecutive empty lines
        let test_cases = vec![
            "Line 1\n\n\n\nLine 2",
            "Line 1\n\nLine 2\n\n\nLine 3",
            "\n\n\nLine 1",
            "Line 1\n\n\n",
            "Line 1\n   \n   \n   \nLine 2", // Lines with only spaces
            "Line 1\n\t\n\t\nLine 2",        // Lines with only tabs
        ];

        for text in test_cases {
            let lines = render_markdown(text, theme);

            // Check that no two consecutive lines are both empty
            let mut prev_was_empty = false;
            for (i, line) in lines.iter().enumerate() {
                let current_is_empty = is_line_empty(line);
                assert!(
                    !(prev_was_empty && current_is_empty),
                    "Found consecutive empty lines at index {} in text: {:?}",
                    i,
                    text
                );
                prev_was_empty = current_is_empty;
            }
        }
    }

    #[test]
    fn test_empty_lines_with_prefix_simulation() {
        let theme = MarkdownTheme::default();

        // Simulate what draw_history does: add prefix to each line
        let text = "Response line 1\n\n\nResponse line 2";
        let md_lines = render_markdown(text, theme);

        // Simulate adding prefix like draw_history does
        let mut lines_with_prefix: Vec<Line> = Vec::new();
        for (i, md_line) in md_lines.into_iter().enumerate() {
            let prefix = if i == 0 { "< " } else { "  " };
            let mut spans = vec![Span::raw(prefix)];
            spans.extend(md_line.spans);
            lines_with_prefix.push(Line::from(spans));
        }

        // Add empty line between messages (like draw_history)
        lines_with_prefix.push(Line::from(""));

        // Now remove consecutive empty lines
        let mut filtered: Vec<Line> = Vec::new();
        let mut prev_was_empty = false;
        for line in lines_with_prefix {
            if is_line_empty(&line) {
                if !prev_was_empty {
                    filtered.push(line);
                }
                prev_was_empty = true;
            } else {
                filtered.push(line);
                prev_was_empty = false;
            }
        }

        // Verify no consecutive empty lines
        let mut prev_was_empty = false;
        for (i, line) in filtered.iter().enumerate() {
            let current_is_empty = is_line_empty(line);
            assert!(
                !(prev_was_empty && current_is_empty),
                "Found consecutive empty lines at index {} after prefix simulation",
                i
            );
            prev_was_empty = current_is_empty;
        }
    }

    #[test]
    fn test_debug_render_output() {
        let theme = MarkdownTheme::default();

        // Test input with multiple empty lines
        let text = "Line 1\n\n\n\nLine 2\n\n\nLine 3";
        let lines = render_markdown(text, theme);

        // Print each line for debugging
        println!("\n=== Rendered lines ===");
        for (i, line) in lines.iter().enumerate() {
            let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let is_empty = is_line_empty(line);
            println!("Line {}: '{}' (empty: {})", i, content, is_empty);
        }

        // Check for consecutive empty lines
        let mut consecutive_count = 0;
        let mut max_consecutive = 0;
        for line in &lines {
            if is_line_empty(line) {
                consecutive_count += 1;
                max_consecutive = max_consecutive.max(consecutive_count);
            } else {
                consecutive_count = 0;
            }
        }

        println!("Max consecutive empty lines: {}", max_consecutive);
        assert!(
            max_consecutive <= 1,
            "Found {} consecutive empty lines",
            max_consecutive
        );
    }

    #[test]
    fn test_full_draw_history_simulation() {
        let theme = MarkdownTheme::default();

        // Simulate multiple messages like in draw_history
        let messages = vec![
            "Hello, this is a question.",
            "This is response 1.\n\n\nWith multiple paragraphs.\n\n\n\nAnd more content.",
            "Another question?",
            "Response 2.\n\nWith a list:\n- Item 1\n\n\n- Item 2",
        ];

        let mut all_lines: Vec<Line> = Vec::new();

        for (msg_idx, content) in messages.iter().enumerate() {
            let is_assistant = msg_idx % 2 == 1;
            let icon = if is_assistant { "< " } else { "> " };

            if is_assistant {
                let md_lines = render_markdown(content, theme);
                for (i, md_line) in md_lines.into_iter().enumerate() {
                    let prefix = if i == 0 { icon } else { "  " };
                    let mut spans = vec![Span::raw(prefix)];
                    spans.extend(md_line.spans);
                    all_lines.push(Line::from(spans));
                }
            } else {
                for (i, line_text) in content.lines().enumerate() {
                    let prefix = if i == 0 { icon } else { "  " };
                    all_lines.push(Line::from(vec![
                        Span::raw(prefix),
                        Span::raw(line_text.to_string()),
                    ]));
                }
            }
            // Empty line between messages
            all_lines.push(Line::from(""));
        }

        println!("\n=== Before filtering ===");
        for (i, line) in all_lines.iter().enumerate() {
            let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            println!("Line {}: '{}' (empty: {})", i, content, is_line_empty(line));
        }

        // Apply consecutive empty line removal
        let mut filtered: Vec<Line> = Vec::new();
        let mut prev_was_empty = false;
        for line in all_lines {
            if is_line_empty(&line) {
                if !prev_was_empty {
                    filtered.push(line);
                }
                prev_was_empty = true;
            } else {
                filtered.push(line);
                prev_was_empty = false;
            }
        }

        println!("\n=== After filtering ===");
        for (i, line) in filtered.iter().enumerate() {
            let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            println!("Line {}: '{}' (empty: {})", i, content, is_line_empty(line));
        }

        // Verify no consecutive empty lines
        let mut consecutive_count = 0;
        let mut max_consecutive = 0;
        for line in &filtered {
            if is_line_empty(line) {
                consecutive_count += 1;
                max_consecutive = max_consecutive.max(consecutive_count);
            } else {
                consecutive_count = 0;
            }
        }

        println!("\nMax consecutive empty lines: {}", max_consecutive);
        assert!(
            max_consecutive <= 1,
            "Found {} consecutive empty lines",
            max_consecutive
        );
    }

    #[test]
    fn test_korean_greeting_response() {
        let theme = MarkdownTheme::default();

        // Simulate typical AI response to "안녕"
        let ai_response = r#"안녕하세요! 반갑습니다.

무엇을 도와드릴까요? 파일 관리나 다른 작업이 필요하시면 말씀해주세요.

다음과 같은 작업을 도와드릴 수 있습니다:
- 파일 복사/이동/삭제
- 디렉토리 탐색
- 파일 검색
- 기타 터미널 작업"#;

        let lines = render_markdown(ai_response, theme);

        println!("\n=== Korean greeting response rendering ===");
        for (i, line) in lines.iter().enumerate() {
            let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let is_empty = is_line_empty(line);
            println!("Line {}: '{}' (empty: {})", i, content, is_empty);
        }

        // Check no consecutive empty lines
        let mut consecutive = 0;
        let mut max = 0;
        for line in &lines {
            if is_line_empty(line) {
                consecutive += 1;
                max = max.max(consecutive);
            } else {
                consecutive = 0;
            }
        }

        println!(
            "\nTotal lines: {}, Max consecutive empty: {}",
            lines.len(),
            max
        );
        assert!(max <= 1, "Found {} consecutive empty lines", max);
    }

    #[test]
    fn test_korean_full_conversation_simulation() {
        let theme = MarkdownTheme::default();

        // User: "안녕."
        let user_input = "안녕.";

        // AI Response
        let ai_response = r#"안녕하세요! 반갑습니다.

무엇을 도와드릴까요? 파일 관리나 다른 작업이 필요하시면 말씀해주세요.

다음과 같은 작업을 도와드릴 수 있습니다:
- 파일 복사/이동/삭제
- 디렉토리 탐색
- 파일 검색
- 기타 터미널 작업"#;

        // Simulate draw_history
        let mut all_lines: Vec<Line> = Vec::new();

        // User message
        all_lines.push(Line::from(vec![Span::raw("> "), Span::raw(user_input)]));
        all_lines.push(Line::from("")); // Empty line between messages

        // AI response with markdown
        let md_lines = render_markdown(ai_response, theme);
        for (i, md_line) in md_lines.into_iter().enumerate() {
            let prefix = if i == 0 { "< " } else { "  " };
            let mut spans = vec![Span::raw(prefix)];
            spans.extend(md_line.spans);
            all_lines.push(Line::from(spans));
        }
        all_lines.push(Line::from("")); // Empty line after message

        println!("\n=== Before filtering ===");
        for (i, line) in all_lines.iter().enumerate() {
            let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            println!("Line {}: '{}' (empty: {})", i, content, is_line_empty(line));
        }

        // Apply consecutive empty line removal
        let mut filtered: Vec<Line> = Vec::new();
        let mut prev_was_empty = false;
        for line in all_lines {
            if is_line_empty(&line) {
                if !prev_was_empty {
                    filtered.push(line);
                }
                prev_was_empty = true;
            } else {
                filtered.push(line);
                prev_was_empty = false;
            }
        }

        println!("\n=== After filtering (final display) ===");
        for (i, line) in filtered.iter().enumerate() {
            let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            println!("Line {}: '{}' (empty: {})", i, content, is_line_empty(line));
        }

        // Verify no consecutive empty lines
        let mut consecutive = 0;
        let mut max = 0;
        for line in &filtered {
            if is_line_empty(line) {
                consecutive += 1;
                max = max.max(consecutive);
            } else {
                consecutive = 0;
            }
        }

        println!(
            "\nTotal lines: {}, Max consecutive empty: {}",
            filtered.len(),
            max
        );
        assert!(max <= 1, "Found {} consecutive empty lines", max);
    }

    #[test]
    fn test_paragraph_wrap_empty_lines() {
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::widgets::{Paragraph, Wrap};
        use ratatui::Terminal;

        // Test NBSP behavior
        let nbsp = "\u{00A0}";
        println!("\n=== NBSP Analysis ===");
        println!("NBSP: {:?}", nbsp);
        println!("NBSP trim: {:?}", nbsp.trim());
        println!("NBSP trim is_empty: {}", nbsp.trim().is_empty());
        println!(
            "NBSP is_whitespace: {}",
            nbsp.chars().all(|c| c.is_whitespace())
        );

        // Test NBSP lines
        let lines: Vec<Line> = vec![
            Line::from(vec![Span::raw("< "), Span::raw("Content 1")]),
            Line::from("\u{00A0}"), // NBSP - should render as 1 row
            Line::from(vec![Span::raw("  "), Span::raw("Content 2")]),
            Line::from("\u{00A0}"), // NBSP - should render as 1 row
            Line::from(vec![Span::raw("  "), Span::raw("Content 3")]),
            Line::from("\u{00A0}"), // NBSP - should render as 1 row
        ];

        println!("\n=== Input Lines ===");
        for (i, line) in lines.iter().enumerate() {
            let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            println!(
                "Line {}: '{}' (spans: {}, empty: {})",
                i,
                content,
                line.spans.len(),
                is_line_empty(line)
            );
        }

        // Render with Paragraph and Wrap
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 40, 10);
                let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
                frame.render_widget(paragraph, area);
            })
            .unwrap();

        println!("\n=== Rendered Output ===");
        let buffer = terminal.backend().buffer();
        let mut consecutive = 0;
        let mut max = 0;
        for y in 0..10u16 {
            let mut line = String::new();
            for x in 0..40u16 {
                let cell = buffer.get(x, y);
                line.push_str(cell.symbol());
            }
            let trimmed = line.trim_end();
            let is_empty = trimmed.is_empty();
            if is_empty {
                consecutive += 1;
                max = max.max(consecutive);
            } else {
                consecutive = 0;
            }
            println!("Row {}: '{}' (empty: {})", y, trimmed, is_empty);
        }
        println!("\nMax consecutive empty rows: {}", max);
    }

    #[test]
    fn test_real_claude_response_simulation() {
        let theme = MarkdownTheme::default();

        // Actual Claude response (captured from --prompt)
        let raw_response = r#"안녕하세요! 터미널 파일 관리자 도우미입니다.

현재 작업 디렉토리는 /mnt/hgfs/vmware_ubuntu_shared/cokacdir_rust 입니다.

어떤 파일 작업을 도와드릴까요? 예를 들어:
- 파일/폴더 목록 보기
- 파일 복사, 이동, 이름 변경
- 디렉토리 생성
- 파일 내용 보기"#;

        // Step 1: Normalize (like add_to_history does)
        fn normalize_empty_lines(text: &str) -> String {
            let lines: Vec<&str> = text.lines().collect();
            let mut result_lines: Vec<&str> = Vec::new();
            let mut prev_was_empty = false;

            for line in lines {
                let is_empty = line.chars().all(|c| c.is_whitespace());
                if is_empty {
                    if !prev_was_empty {
                        result_lines.push("");
                    }
                    prev_was_empty = true;
                } else {
                    result_lines.push(line);
                    prev_was_empty = false;
                }
            }
            result_lines.join("\n")
        }

        let normalized = normalize_empty_lines(raw_response);
        println!("\n=== After normalize_empty_lines ===");
        for (i, line) in normalized.lines().enumerate() {
            println!("Normalized line {}: '{}'", i, line);
        }

        // Step 2: Render markdown
        let md_lines = render_markdown(&normalized, theme);
        println!("\n=== After render_markdown ===");
        for (i, line) in md_lines.iter().enumerate() {
            let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            println!(
                "MD line {}: '{}' (empty: {})",
                i,
                content,
                is_line_empty(line)
            );
        }

        // Step 3: Add prefix (like draw_history does)
        let mut lines_with_prefix: Vec<Line> = Vec::new();
        for (i, md_line) in md_lines.into_iter().enumerate() {
            let prefix = if i == 0 { "< " } else { "  " };
            let mut spans = vec![Span::raw(prefix)];
            spans.extend(md_line.spans);
            lines_with_prefix.push(Line::from(spans));
        }
        // Add empty line after message
        lines_with_prefix.push(Line::from(""));

        println!("\n=== After adding prefix ===");
        for (i, line) in lines_with_prefix.iter().enumerate() {
            let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            println!(
                "Prefix line {}: '{}' (empty: {})",
                i,
                content,
                is_line_empty(line)
            );
        }

        // Step 4: Remove consecutive empty lines
        let mut filtered: Vec<Line> = Vec::new();
        let mut prev_was_empty = false;
        for line in lines_with_prefix {
            if is_line_empty(&line) {
                if !prev_was_empty {
                    filtered.push(line);
                }
                prev_was_empty = true;
            } else {
                filtered.push(line);
                prev_was_empty = false;
            }
        }

        println!("\n=== Final filtered lines ===");
        for (i, line) in filtered.iter().enumerate() {
            let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            println!(
                "Final line {}: '{}' (empty: {})",
                i,
                content,
                is_line_empty(line)
            );
        }

        // Verify no consecutive empty lines
        let mut consecutive = 0;
        let mut max = 0;
        for line in &filtered {
            if is_line_empty(line) {
                consecutive += 1;
                max = max.max(consecutive);
            } else {
                consecutive = 0;
            }
        }

        println!("\nMax consecutive empty: {}", max);
        assert!(max <= 1, "Found {} consecutive empty lines", max);
    }

    #[test]
    fn test_special_markdown_patterns() {
        let theme = MarkdownTheme::default();

        // Various markdown patterns that might cause issues
        let test_cases = vec![
            // Headers with empty lines
            "# Header 1\n\n\n## Header 2",
            // Code blocks with empty lines
            "```rust\ncode\n\n\nmore code\n```\n\n\nafter code",
            // Lists with empty lines between items
            "- Item 1\n\n\n- Item 2\n\n\n- Item 3",
            // Mixed content
            "# Title\n\n\nParagraph\n\n\n```\ncode\n```\n\n\n- list item",
            // Blockquotes
            "> Quote 1\n\n\n> Quote 2",
            // Horizontal rules
            "---\n\n\n---",
            // Tables
            "| A | B |\n|---|---|\n| 1 | 2 |\n\n\n| C | D |\n|---|---|\n| 3 | 4 |",
        ];

        for (idx, text) in test_cases.iter().enumerate() {
            let lines = render_markdown(text, theme);

            // Check for consecutive empty lines
            let mut consecutive = 0;
            let mut max = 0;
            for line in &lines {
                if is_line_empty(line) {
                    consecutive += 1;
                    max = max.max(consecutive);
                } else {
                    consecutive = 0;
                }
            }

            assert!(
                max <= 1,
                "Test case {} failed: found {} consecutive empty lines.\nInput: {:?}",
                idx,
                max,
                text
            );
        }
    }
}
