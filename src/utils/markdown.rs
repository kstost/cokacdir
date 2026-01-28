use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

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
                        Span::styled(
                            code_line.clone(),
                            Style::default().fg(theme_colors.code),
                        ),
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
        if line.trim().chars().all(|c| c == '-' || c == '*' || c == '_')
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
        if let Some(rest) = line.strip_prefix("- [ ] ").or_else(|| line.strip_prefix("* [ ] ")) {
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
        if let Some(rest) = line.strip_prefix("- [x] ")
            .or_else(|| line.strip_prefix("* [x] ")
            .or_else(|| line.strip_prefix("- [X] ")
            .or_else(|| line.strip_prefix("* [X] "))))
        {
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
            let bullet = if indent_level == 0 { "• " } else if indent_level == 1 { "◦ " } else { "▪ " };
            let spans = parse_inline_markdown(content, &theme_colors);
            let mut result = vec![
                Span::styled(format!("{}{}", indent, bullet), Style::default().fg(theme_colors.text)),
            ];
            result.extend(spans);
            lines.push(Line::from(result));
            i += 1;
            continue;
        }

        // Handle unordered list
        if let Some(content) = line.strip_prefix("- ")
            .or_else(|| line.strip_prefix("* ")
            .or_else(|| line.strip_prefix("+ ")))
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
                Span::styled(
                    code_line.clone(),
                    Style::default().fg(theme_colors.code),
                ),
            ]));
        }
    }

    lines
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
            cell.chars().all(|c| c == '-' || c == ':' || c == ' ')
                && cell.contains('-')
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
                    col_widths[col_idx] = col_widths[col_idx].max(UnicodeWidthStr::width(cell.as_str()));
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
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled("│", Style::default().fg(theme.dim)),
        ];

        for (col_idx, width) in col_widths.iter().enumerate() {
            let cell_content = row.get(col_idx).map(|s| s.as_str()).unwrap_or("");
            // Calculate padding using unicode width
            let cell_width = UnicodeWidthStr::width(cell_content);
            let padding = width.saturating_sub(cell_width);
            let padded = format!(" {}{} ", cell_content, " ".repeat(padding));

            // Header row (before separator) gets bold style
            let is_header = separator_idx.map(|idx| row_idx < idx).unwrap_or(false);
            let style = if is_header {
                Style::default().fg(theme.header).add_modifier(Modifier::BOLD)
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
                    Style::default()
                        .fg(theme.text)
                        .add_modifier(Modifier::BOLD),
                ));
                current_pos = end + 2;
                continue;
            }
        }

        // Check for inline code (`code`)
        if chars[current_pos] == '`' {
            if let Some(end) = find_closing_char(&chars, current_pos + 1, '`') {
                let content: String = chars[current_pos + 1..end].iter().collect();
                spans.push(Span::styled(
                    content,
                    Style::default().fg(theme.code),
                ));
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
        if chars[current_pos] == '[' {
            if let Some((link_text, url, end_pos)) = parse_link(&chars, current_pos) {
                spans.push(Span::styled(
                    link_text,
                    Style::default().fg(theme.link).add_modifier(Modifier::UNDERLINED),
                ));
                spans.push(Span::styled(
                    format!(" ({})", url),
                    Style::default().fg(theme.dim),
                ));
                current_pos = end_pos;
                continue;
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

fn parse_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    // Find closing bracket
    let mut bracket_end = None;
    for i in start + 1..chars.len() {
        if chars[i] == ']' {
            bracket_end = Some(i);
            break;
        }
    }
    let bracket_end = bracket_end?;

    // Check for opening parenthesis
    if bracket_end + 1 >= chars.len() || chars[bracket_end + 1] != '(' {
        return None;
    }

    // Find closing parenthesis
    let mut paren_end = None;
    for i in bracket_end + 2..chars.len() {
        if chars[i] == ')' {
            paren_end = Some(i);
            break;
        }
    }
    let paren_end = paren_end?;

    let link_text: String = chars[start + 1..bracket_end].iter().collect();
    let url: String = chars[bracket_end + 2..paren_end].iter().collect();

    Some((link_text, url, paren_end + 1))
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
    pub fn from_theme(theme: &crate::ui::theme::Theme) -> Self {
        Self {
            text: theme.text,
            dim: theme.text_dim,
            header: theme.info,
            code: theme.warning,
            link: theme.info,
            blockquote: theme.text_header,
            success: theme.success,
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
}
