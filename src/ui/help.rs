//! Help screen with scrolling support
//!
//! Provides a comprehensive help dialog showing all keyboard shortcuts
//! and features of the application.

use crossterm::event::KeyCode;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

use super::{app::App, draw::draw_panel_background, theme::Theme};
use crate::keybindings::{
    AIScreenAction, DiffFileViewAction, DiffScreenAction, EditorAction, ImageViewerAction,
    Keybindings, PanelAction, ProcessManagerAction, SearchResultAction,
};

/// Draw the help screen
pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, theme: &Theme) {
    // First draw the panels in background
    draw_panel_background(frame, app, area, theme);

    // Build help content
    let lines = build_help_content(theme, &app.keybindings);
    let total_lines = lines.len();

    // Calculate dialog size (max 85% of screen, within bounds).
    // Clamp to the terminal area LAST: the dialog must never exceed the buffer, or
    // Clear/Paragraph render out of bounds and panic on small terminals.
    let width = ((area.width as u32 * 85 / 100) as u16)
        .min(100)
        .max(50)
        .min(area.width);
    let height = ((area.height as u32 * 85 / 100) as u16)
        .min(50)
        .max(20)
        .min(area.height);

    // Need minimum size to display anything useful
    if width < 30 || height < 10 {
        return;
    }

    // Calculate visible height (excluding borders)
    let visible_height = (height.saturating_sub(2)) as usize;
    let max_scroll = total_lines.saturating_sub(visible_height);

    // Update state
    app.help_state.visible_height = visible_height;
    app.help_state.max_scroll = max_scroll;
    app.help_state.scroll_offset = app.help_state.scroll_offset.min(max_scroll);

    // Calculate dialog position (centered)
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    // Clear the area
    frame.render_widget(Clear, dialog_area);

    // Create block
    let block = Block::default()
        .title(" Help ")
        .title_style(
            Style::default()
                .fg(theme.help.title)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.help.border))
        .style(Style::default().bg(theme.help.bg));

    // Render paragraph with scroll
    let paragraph = Paragraph::new(lines)
        .block(block)
        .scroll((app.help_state.scroll_offset as u16, 0));

    frame.render_widget(paragraph, dialog_area);

    // Render scrollbar if content exceeds visible height
    if total_lines > visible_height {
        let scrollbar_area = Rect::new(
            dialog_area.x + dialog_area.width - 1,
            dialog_area.y + 1,
            1,
            dialog_area.height.saturating_sub(2),
        );

        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("^"))
            .end_symbol(Some("v"));

        let mut scrollbar_state =
            ScrollbarState::new(max_scroll + 1).position(app.help_state.scroll_offset);

        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }
}

/// Handle keyboard input for help screen
/// Returns true if the screen should be closed
pub fn handle_input(app: &mut App, code: KeyCode) -> bool {
    let state = &mut app.help_state;

    match code {
        // Scroll up
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
            state.scroll_offset = state.scroll_offset.saturating_sub(1);
            false
        }
        // Scroll down
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
            if state.scroll_offset < state.max_scroll {
                state.scroll_offset += 1;
            }
            false
        }
        // Page up
        KeyCode::PageUp => {
            let amount = state.visible_height.saturating_sub(1).max(1);
            state.scroll_offset = state.scroll_offset.saturating_sub(amount);
            false
        }
        // Page down
        KeyCode::PageDown => {
            let amount = state.visible_height.saturating_sub(1).max(1);
            state.scroll_offset = (state.scroll_offset + amount).min(state.max_scroll);
            false
        }
        // Go to top
        KeyCode::Home | KeyCode::Char('g') => {
            state.scroll_offset = 0;
            false
        }
        // Go to bottom
        KeyCode::End | KeyCode::Char('G') => {
            state.scroll_offset = state.max_scroll;
            false
        }
        // Close help screen
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Char('?') => {
            state.scroll_offset = 0; // Reset scroll for next time
            true
        }
        _ => false,
    }
}

/// Build the help content as styled lines
fn build_help_content(theme: &Theme, kb: &Keybindings) -> Vec<Line<'static>> {
    // Content width is passed implicitly; we use a fixed wrap width for quick reference
    // that matches the expanded dialog (100 max - 2 borders = 98, minus some padding)
    let qr_wrap_width: usize = 96;
    let section_title_style = Style::default()
        .fg(theme.help.section_title)
        .add_modifier(Modifier::BOLD);
    let section_decorator_style = Style::default().fg(theme.help.section_decorator);
    let key_style = Style::default().fg(theme.help.key);
    let key_highlight_style = Style::default().fg(theme.help.key_highlight);
    let desc_style = Style::default().fg(theme.help.description);
    let hint_style = Style::default().fg(theme.help.hint_text);

    let mut lines: Vec<Line> = Vec::new();

    // Helper to create section header
    let section = |title: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled("── ".to_string(), section_decorator_style),
            Span::styled(title.to_string(), section_title_style),
            Span::styled(" ──".to_string(), section_decorator_style),
        ])
    };

    // Helper to create key-description line (static strings)
    let key_line = |key: &str, desc: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {:28}", key), key_style),
            Span::styled(desc.to_string(), desc_style),
        ])
    };

    // Helper to create key-description line from a PanelAction (reads from keybindings)
    let pk = |action: PanelAction, desc: &str| -> Line<'static> {
        let key_display = kb.panel_first_key(action).to_string();
        Line::from(vec![
            Span::styled(format!("  {:28}", key_display), key_style),
            Span::styled(desc.to_string(), desc_style),
        ])
    };

    // ═══════════════════════════════════════════════════════════════════════
    // Section 1: Navigation
    // ═══════════════════════════════════════════════════════════════════════
    lines.push(section("Navigation"));
    lines.push(pk(PanelAction::MoveUp, "Move cursor up"));
    lines.push(pk(PanelAction::MoveDown, "Move cursor down"));
    lines.push(pk(PanelAction::PageUp, "Page up"));
    lines.push(pk(PanelAction::PageDown, "Page down"));
    lines.push(pk(PanelAction::GoHome, "Go to first item"));
    lines.push(pk(PanelAction::GoEnd, "Go to last item"));
    lines.push(pk(PanelAction::Open, "Open directory or file"));
    lines.push(pk(PanelAction::ParentDir, "Go to parent directory"));
    lines.push(pk(PanelAction::SwitchPanel, "Switch panel"));
    lines.push(pk(PanelAction::SwitchPanelLeft, "Switch to left panel"));
    lines.push(pk(PanelAction::SwitchPanelRight, "Switch to right panel"));
    lines.push(pk(PanelAction::GoHomeDir, "Go to home directory"));
    lines.push(pk(PanelAction::Refresh, "Refresh file list"));
    lines.push(pk(PanelAction::GoToPath, "Go to path dialog"));
    lines.push(pk(PanelAction::ToggleBookmark, "Toggle bookmark"));
    lines.push(pk(PanelAction::AddPanel, "Add new panel"));
    lines.push(pk(PanelAction::ClosePanel, "Close current panel"));
    lines.push(Line::from(""));

    // ═══════════════════════════════════════════════════════════════════════
    // Section 2: Selection & Marking
    // ═══════════════════════════════════════════════════════════════════════
    lines.push(section("Selection & Marking"));
    lines.push(pk(PanelAction::ToggleSelect, "Select/deselect file"));
    lines.push(pk(PanelAction::SelectAll, "Select/deselect all"));
    lines.push(pk(PanelAction::SelectUp, "Select and move up"));
    lines.push(pk(PanelAction::SelectDown, "Select and move down"));
    lines.push(pk(PanelAction::SelectByExtension, "Select by extension"));
    lines.push(Line::from(vec![
        Span::styled("  ".to_string(), desc_style),
        Span::styled("Selected files are marked with ".to_string(), hint_style),
        Span::styled(
            "*".to_string(),
            Style::default().fg(theme.panel.marked_text),
        ),
    ]));
    lines.push(Line::from(""));

    // ═══════════════════════════════════════════════════════════════════════
    // Section 3: Sorting
    // ═══════════════════════════════════════════════════════════════════════
    lines.push(section("Sorting"));
    lines.push(pk(PanelAction::SortByName, "Sort by name"));
    lines.push(pk(PanelAction::SortBySize, "Sort by size"));
    lines.push(pk(PanelAction::SortByDate, "Sort by date"));
    lines.push(pk(PanelAction::SortByType, "Sort by type (extension)"));
    lines.push(Line::from(vec![
        Span::styled("  ".to_string(), desc_style),
        Span::styled("Press again to toggle Asc/Desc".to_string(), hint_style),
    ]));
    lines.push(Line::from(""));

    // ═══════════════════════════════════════════════════════════════════════
    // Section 4: File Operations
    // ═══════════════════════════════════════════════════════════════════════
    lines.push(section("File Operations"));
    lines.push(pk(PanelAction::Edit, "Edit file"));
    lines.push(pk(PanelAction::FileInfo, "File info (properties)"));
    lines.push(pk(PanelAction::Mkdir, "Create new directory"));
    lines.push(pk(PanelAction::Mkfile, "Create new file"));
    lines.push(pk(PanelAction::Rename, "Rename file/directory"));
    lines.push(pk(PanelAction::Tar, "Create tar archive"));
    lines.push(pk(PanelAction::SetHandler, "Set/Edit file handler"));
    lines.push(pk(PanelAction::Delete, "Delete file(s)"));
    lines.push(pk(PanelAction::EncryptAll, "Encrypt all files (AES-256)"));
    lines.push(pk(PanelAction::DecryptAll, "Decrypt .cokacenc files"));
    lines.push(pk(PanelAction::Search, "Find/search files"));
    #[cfg(target_os = "macos")]
    {
        lines.push(pk(PanelAction::OpenInFinder, "Open folder in Finder"));
        lines.push(pk(PanelAction::OpenInVSCode, "Open folder in VS Code"));
    }
    #[cfg(target_os = "windows")]
    {
        lines.push(pk(PanelAction::OpenInExplorer, "Open folder in Explorer"));
        lines.push(pk(PanelAction::OpenInVSCode, "Open folder in VS Code"));
    }
    lines.push(Line::from(""));

    // ═══════════════════════════════════════════════════════════════════════
    // Section 5: Clipboard
    // ═══════════════════════════════════════════════════════════════════════
    lines.push(section("Clipboard"));
    lines.push(pk(PanelAction::Copy, "Copy to clipboard"));
    lines.push(pk(PanelAction::Cut, "Cut to clipboard"));
    lines.push(pk(PanelAction::Paste, "Paste from clipboard"));
    lines.push(Line::from(vec![
        Span::styled("  ".to_string(), desc_style),
        Span::styled(
            "Conflict resolution: Overwrite/Skip/All".to_string(),
            hint_style,
        ),
    ]));
    lines.push(Line::from(""));

    // ═══════════════════════════════════════════════════════════════════════
    // Section 6: File Editor
    // ═══════════════════════════════════════════════════════════════════════
    // Helper to create key-description line from an EditorAction
    let ek = |action: EditorAction, desc: &str| -> Line<'static> {
        let key_display = kb.editor_first_key(action).to_string();
        Line::from(vec![
            Span::styled(format!("  {:28}", key_display), key_style),
            Span::styled(desc.to_string(), desc_style),
        ])
    };

    lines.push(section("File Editor"));
    lines.push(key_line("Arrows", "Move cursor"));
    lines.push(key_line("Home/End", "Go to line start/end"));
    lines.push(ek(EditorAction::GoToFileStart, "Go to file start"));
    lines.push(ek(EditorAction::GoToFileEnd, "Go to file end"));
    lines.push(key_line("Shift+Arrows", "Select text"));
    lines.push(ek(EditorAction::SelectAll, "Select all"));
    lines.push(ek(EditorAction::Copy, "Copy (line if no selection)"));
    lines.push(ek(EditorAction::Cut, "Cut (line if no selection)"));
    lines.push(ek(EditorAction::Paste, "Paste"));
    lines.push(ek(EditorAction::SelectNextOccurrence, "Select word"));
    lines.push(ek(EditorAction::SelectLine, "Select line"));
    lines.push(ek(EditorAction::DeleteLine, "Delete line"));
    lines.push(ek(EditorAction::DuplicateLine, "Duplicate line"));
    lines.push(ek(EditorAction::ToggleComment, "Toggle comment"));
    lines.push(ek(EditorAction::MoveLineUp, "Move line up"));
    lines.push(ek(EditorAction::MoveLineDown, "Move line down"));
    lines.push(ek(EditorAction::Undo, "Undo"));
    lines.push(ek(EditorAction::Redo, "Redo"));
    lines.push(ek(EditorAction::Find, "Find text"));
    lines.push(ek(EditorAction::Replace, "Find and replace"));
    lines.push(ek(EditorAction::GotoLine, "Go to line"));
    lines.push(ek(EditorAction::Save, "Save file"));
    lines.push(ek(EditorAction::Exit, "Close editor"));
    lines.push(Line::from(""));

    // ═══════════════════════════════════════════════════════════════════════
    // Section 8: Image Viewer
    // ═══════════════════════════════════════════════════════════════════════
    // Helper to create key-description line from an ImageViewerAction
    let ivk = |action: ImageViewerAction, desc: &str| -> Line<'static> {
        let key_display = kb.image_viewer_first_key(action).to_string();
        Line::from(vec![
            Span::styled(format!("  {:28}", key_display), key_style),
            Span::styled(desc.to_string(), desc_style),
        ])
    };

    lines.push(section("Image Viewer"));
    lines.push(ivk(ImageViewerAction::ZoomIn, "Zoom in"));
    lines.push(ivk(ImageViewerAction::ZoomOut, "Zoom out"));
    lines.push(ivk(ImageViewerAction::ResetView, "Reset zoom"));
    lines.push(ivk(ImageViewerAction::PanUp, "Pan up"));
    lines.push(ivk(ImageViewerAction::PanDown, "Pan down"));
    lines.push(ivk(ImageViewerAction::PrevImage, "Previous image"));
    lines.push(ivk(ImageViewerAction::NextImage, "Next image"));
    lines.push(ivk(ImageViewerAction::Close, "Close viewer"));
    lines.push(Line::from(""));

    // ═══════════════════════════════════════════════════════════════════════
    // Section 9: Process Manager
    // ═══════════════════════════════════════════════════════════════════════
    // Helper to create key-description line from a ProcessManagerAction
    let pmk = |action: ProcessManagerAction, desc: &str| -> Line<'static> {
        let key_display = kb.process_manager_first_key(action).to_string();
        Line::from(vec![
            Span::styled(format!("  {:28}", key_display), key_style),
            Span::styled(desc.to_string(), desc_style),
        ])
    };

    lines.push(section("Process Manager"));
    lines.push(pmk(ProcessManagerAction::MoveUp, "Navigate up"));
    lines.push(pmk(ProcessManagerAction::MoveDown, "Navigate down"));
    lines.push(pmk(ProcessManagerAction::PageUp, "Page up"));
    lines.push(pmk(ProcessManagerAction::PageDown, "Page down"));
    lines.push(pmk(ProcessManagerAction::SortByPid, "Sort by PID"));
    lines.push(pmk(ProcessManagerAction::SortByCpu, "Sort by CPU usage"));
    lines.push(pmk(ProcessManagerAction::SortByMem, "Sort by memory usage"));
    lines.push(pmk(ProcessManagerAction::SortByName, "Sort by name"));
    lines.push(pmk(ProcessManagerAction::Kill, "Kill process (SIGTERM)"));
    lines.push(pmk(ProcessManagerAction::ForceKill, "Force kill (SIGKILL)"));
    lines.push(pmk(ProcessManagerAction::Refresh, "Refresh list"));
    lines.push(pmk(ProcessManagerAction::Quit, "Close manager"));
    lines.push(Line::from(""));

    // ═══════════════════════════════════════════════════════════════════════
    // Section 10: AI Assistant
    // ═══════════════════════════════════════════════════════════════════════
    if !kb.panel_first_key(PanelAction::AIScreen).is_empty() {
        lines.push(section("AI Assistant"));
        let aik = |action: AIScreenAction, desc: &str| -> Line<'static> {
            let key_display = kb.ai_screen_first_key(action).to_string();
            Line::from(vec![
                Span::styled(format!("  {:28}", key_display), key_style),
                Span::styled(desc.to_string(), desc_style),
            ])
        };
        lines.push(pk(PanelAction::AIScreen, "Open AI assistant"));
        lines.push(aik(AIScreenAction::Submit, "Send message"));
        lines.push(aik(AIScreenAction::InsertNewline, "New line in input"));
        lines.push(aik(AIScreenAction::ScrollHistoryUp, "Scroll response up"));
        lines.push(aik(
            AIScreenAction::ScrollHistoryDown,
            "Scroll response down",
        ));
        lines.push(aik(AIScreenAction::PageUp, "Page scroll up"));
        lines.push(aik(AIScreenAction::PageDown, "Page scroll down"));
        lines.push(aik(AIScreenAction::ClearHistory, "Clear conversation"));
        lines.push(aik(AIScreenAction::ToggleFullscreen, "Toggle fullscreen"));
        lines.push(aik(AIScreenAction::Escape, "Close assistant"));
        lines.push(Line::from(""));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Section 11: Search
    // ═══════════════════════════════════════════════════════════════════════
    // Helper to create key-description line from a SearchResultAction
    let srk = |action: SearchResultAction, desc: &str| -> Line<'static> {
        let key_display = kb.search_result_first_key(action).to_string();
        Line::from(vec![
            Span::styled(format!("  {:28}", key_display), key_style),
            Span::styled(desc.to_string(), desc_style),
        ])
    };

    lines.push(section("Search"));
    lines.push(pk(PanelAction::Search, "Open search dialog"));
    lines.push(srk(SearchResultAction::MoveUp, "Navigate up"));
    lines.push(srk(SearchResultAction::MoveDown, "Navigate down"));
    lines.push(srk(SearchResultAction::Open, "Go to selected result"));
    lines.push(srk(SearchResultAction::Close, "Close search"));
    lines.push(Line::from(""));

    // ═══════════════════════════════════════════════════════════════════════
    // Section 12: Diff Compare
    // ═══════════════════════════════════════════════════════════════════════
    // Helper to create key-description line from a DiffScreenAction
    let dsk = |action: DiffScreenAction, desc: &str| -> Line<'static> {
        let key_display = kb.diff_screen_first_key(action).to_string();
        Line::from(vec![
            Span::styled(format!("  {:28}", key_display), key_style),
            Span::styled(desc.to_string(), desc_style),
        ])
    };

    lines.push(section("Diff Compare"));
    lines.push(pk(PanelAction::StartDiff, "Start folder diff (2 panels)"));
    lines.push(Line::from(vec![
        Span::styled("  ".to_string(), desc_style),
        Span::styled(
            "3+ panels: press twice to select pair".to_string(),
            hint_style,
        ),
    ]));
    lines.push(dsk(DiffScreenAction::MoveUp, "Move cursor up"));
    lines.push(dsk(DiffScreenAction::MoveDown, "Move cursor down"));
    lines.push(dsk(DiffScreenAction::PageUp, "Page up"));
    lines.push(dsk(DiffScreenAction::PageDown, "Page down"));
    lines.push(dsk(DiffScreenAction::GoHome, "Go to first item"));
    lines.push(dsk(DiffScreenAction::GoEnd, "Go to last item"));
    lines.push(dsk(DiffScreenAction::Open, "View file content diff"));
    lines.push(dsk(DiffScreenAction::ToggleSelect, "Select/deselect item"));
    lines.push(dsk(
        DiffScreenAction::CycleFilter,
        "Cycle filter (All/Diff/L/R)",
    ));
    lines.push(dsk(DiffScreenAction::SortByName, "Sort by name"));
    lines.push(dsk(DiffScreenAction::SortBySize, "Sort by size"));
    lines.push(dsk(DiffScreenAction::SortByDate, "Sort by date"));
    lines.push(dsk(DiffScreenAction::SortByType, "Sort by type"));
    lines.push(dsk(DiffScreenAction::ExpandDir, "Expand directory"));
    lines.push(dsk(DiffScreenAction::CollapseDir, "Collapse directory"));
    lines.push(dsk(DiffScreenAction::ExpandAll, "Expand all"));
    lines.push(dsk(DiffScreenAction::CollapseAll, "Collapse all"));
    lines.push(dsk(DiffScreenAction::Close, "Return to file panel"));
    lines.push(Line::from(""));

    // ═══════════════════════════════════════════════════════════════════════
    // Section 12b: File Content Diff
    // ═══════════════════════════════════════════════════════════════════════
    // Helper to create key-description line from a DiffFileViewAction
    let dfk = |action: DiffFileViewAction, desc: &str| -> Line<'static> {
        let key_display = kb.diff_file_view_first_key(action).to_string();
        Line::from(vec![
            Span::styled(format!("  {:28}", key_display), key_style),
            Span::styled(desc.to_string(), desc_style),
        ])
    };

    lines.push(section("File Content Diff"));
    lines.push(dfk(DiffFileViewAction::MoveUp, "Scroll up"));
    lines.push(dfk(DiffFileViewAction::MoveDown, "Scroll down"));
    lines.push(dfk(DiffFileViewAction::PageUp, "Page up"));
    lines.push(dfk(DiffFileViewAction::PageDown, "Page down"));
    lines.push(dfk(DiffFileViewAction::GoHome, "Go to start"));
    lines.push(dfk(DiffFileViewAction::GoEnd, "Go to end"));
    lines.push(dfk(DiffFileViewAction::NextChange, "Jump to next change"));
    lines.push(dfk(
        DiffFileViewAction::PrevChange,
        "Jump to previous change",
    ));
    lines.push(dfk(DiffFileViewAction::Close, "Return to diff screen"));
    lines.push(Line::from(""));

    // ═══════════════════════════════════════════════════════════════════════
    // Section 13: Settings
    // ═══════════════════════════════════════════════════════════════════════
    lines.push(section("Settings"));
    lines.push(pk(PanelAction::Settings, "Open settings dialog"));
    lines.push(key_line("Up/Down", "Select setting row"));
    lines.push(key_line("Left/Right", "Change value (theme/diff)"));
    lines.push(key_line("Enter", "Save settings"));
    lines.push(key_line("Esc", "Cancel"));
    lines.push(Line::from(vec![
        Span::styled("  ".to_string(), desc_style),
        Span::styled("Config: ~/.cokacdir/settings.json".to_string(), hint_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  ".to_string(), desc_style),
        Span::styled("Themes: ~/.cokacdir/themes/".to_string(), hint_style),
    ]));
    lines.push(Line::from(""));

    // ═══════════════════════════════════════════════════════════════════════
    // Quick Reference (built from keybindings)
    // ═══════════════════════════════════════════════════════════════════════
    lines.push(section("Quick Reference"));
    {
        let mut qr_items: Vec<(PanelAction, &str)> = vec![
            (PanelAction::Help, "help "),
            (PanelAction::FileInfo, "info "),
            (PanelAction::Edit, "edit "),
            (PanelAction::Mkdir, "mkdir "),
            (PanelAction::Mkfile, "mkfile "),
            (PanelAction::Delete, "del "),
            (PanelAction::Rename, "ren "),
            (PanelAction::Tar, "tar "),
            (PanelAction::Search, "find "),
            (PanelAction::AIScreen, "AI "),
            (PanelAction::ProcessManager, "proc "),
            (PanelAction::GoHomeDir, "home "),
            (PanelAction::Refresh, "ref "),
            (PanelAction::StartDiff, "diff "),
            (PanelAction::AddPanel, "+pan "),
            (PanelAction::ClosePanel, "-pan "),
        ];
        #[cfg(target_os = "macos")]
        {
            qr_items.push((PanelAction::OpenInFinder, "finder "));
            qr_items.push((PanelAction::OpenInVSCode, "vscode "));
        }
        #[cfg(target_os = "windows")]
        {
            qr_items.push((PanelAction::OpenInExplorer, "explorer "));
            qr_items.push((PanelAction::OpenInVSCode, "vscode "));
        }
        qr_items.push((PanelAction::Settings, "set "));
        qr_items.push((PanelAction::Quit, "quit "));

        // Build spans in rows, wrapping at ~70 chars
        let mut row_spans: Vec<Span> = vec![Span::styled("  ".to_string(), desc_style)];
        let mut row_width: usize = 2;
        for (action, label) in &qr_items {
            let key_str = kb.panel_first_key(*action).to_string();
            if key_str.is_empty() {
                continue;
            }
            let entry_width = key_str.len() + 1 + label.len(); // key:label
            if row_width + entry_width > qr_wrap_width && row_width > 2 {
                lines.push(Line::from(std::mem::take(&mut row_spans)));
                row_spans.push(Span::styled("  ".to_string(), desc_style));
                row_width = 2;
            }
            row_spans.push(Span::styled(key_str, key_highlight_style));
            row_spans.push(Span::styled(format!(":{}", label), desc_style));
            row_width += entry_width;
        }
        if row_spans.len() > 1 {
            lines.push(Line::from(row_spans));
        }
    }
    lines.push(Line::from(""));

    // ═══════════════════════════════════════════════════════════════════════
    // Section: Developer Info
    // ═══════════════════════════════════════════════════════════════════════
    lines.push(section("Developer"));
    lines.push(Line::from(vec![
        Span::styled(format!("  {:28}", "Developer"), key_style),
        Span::styled("cokac (코드깎는노인)".to_string(), desc_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled(format!("  {:28}", "Email"), key_style),
        Span::styled("monogatree@gmail.com".to_string(), desc_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled(format!("  {:28}", "Website"), key_style),
        Span::styled("https://cokacdir.cokac.com".to_string(), desc_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled(format!("  {:28}", "YouTube"), key_style),
        Span::styled(
            "https://www.youtube.com/@코드깎는노인".to_string(),
            desc_style,
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(format!("  {:28}", "코깎노클래스"), key_style),
        Span::styled("https://cokac.com/".to_string(), desc_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled(format!("  {:28}", "Tutorial"), key_style),
        Span::styled(
            "https://cokacdir.cokac.com/#/tutorial".to_string(),
            desc_style,
        ),
    ]));
    lines.push(Line::from(""));

    // Footer
    lines.push(Line::from(Span::styled(
        "  Use Up/Down/PgUp/PgDn to scroll. Press Esc or Q to close.",
        hint_style,
    )));

    lines
}
