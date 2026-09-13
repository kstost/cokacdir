//! Mouse routing uses content rectangles from the last draw, never inferred panel widths.
use super::app::{App, Clipboard, ClipboardOperation, Screen};
use crate::keybindings::PanelAction;
use crossterm::{
    event::{
        DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, KeyCode,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
};
use ratatui::layout::Rect;
use std::{
    io,
    time::{Duration, Instant},
};

pub fn enable_capture() -> io::Result<()> {
    execute!(io::stdout(), EnableMouseCapture, EnableFocusChange)
}
pub fn disable_capture() -> io::Result<()> {
    // Try each restoration independently, including after a partial setup failure.
    let mouse = execute!(io::stdout(), DisableMouseCapture);
    let focus = execute!(io::stdout(), DisableFocusChange);
    mouse.and(focus)
}

/// Also runs when terminal initialization or the event loop returns early.
pub struct CaptureGuard;
impl Drop for CaptureGuard {
    fn drop(&mut self) {
        let _ = disable_capture();
    }
}

#[derive(Default)]
pub(crate) struct MouseState {
    frame: Option<(Screen, Rect)>,
    drag: Option<FileDrag>,
    click: Option<FileClick>,
}

struct FileClick {
    panel: u64,
    generation: u64,
    name: String,
    position: (u16, u16),
    time: Instant,
}

struct FileDrag {
    panel: u64,
    generation: u64,
    name: String,
    position: (u16, u16),
    clipboard: Result<Clipboard, String>,
    modifiers: KeyModifiers,
    moved: bool,
}

pub(crate) fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

pub(crate) fn scroll_delta(value: usize, delta: i32) -> usize {
    if delta < 0 {
        value.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        value.saturating_add(delta as usize)
    }
}

fn blocked(app: &App) -> bool {
    app.dialog.is_some()
        || app.remote_spinner.is_some()
        || (app.current_screen == Screen::FilePanel && app.advanced_search_state.active)
        || app
            .file_operation_progress
            .as_ref()
            .is_some_and(|p| p.is_active)
}

pub(crate) fn cancel_gesture(app: &mut App) {
    app.mouse.drag = None;
    app.mouse.click = None;
    if let Some(editor) = app.editor_state.as_mut() {
        editor.cancel_mouse_drag();
    }
}

pub(crate) fn begin_frame(app: &mut App, area: Rect) {
    if app.mouse.frame != Some((app.current_screen, area)) || blocked(app) {
        cancel_gesture(app);
    }
    app.mouse.frame = Some((app.current_screen, area));
    for panel in &mut app.panels {
        panel.mouse_area = None;
    }
    if let Some(editor) = app.editor_state.as_mut() {
        editor.mouse.area = None;
    }
    if let Some(viewer) = app.viewer_state.as_mut() {
        viewer.mouse_area = None;
    }
}

pub(crate) fn before_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    // Modifier-only events do not reach here. Escape or any actual key ends a
    // gesture so a delayed release cannot operate on a newly opened screen.
    cancel_gesture(app);
    if app.current_screen == Screen::FilePanel
        && matches!(
            app.keybindings.panel_action(code, modifiers),
            Some(
                PanelAction::MoveUp
                    | PanelAction::MoveDown
                    | PanelAction::PageUp
                    | PanelAction::PageDown
                    | PanelAction::GoHome
                    | PanelAction::GoEnd
                    | PanelAction::SelectUp
                    | PanelAction::SelectDown
            )
        )
    {
        app.active_panel_mut().mouse_scroll = false;
    }
}

pub(crate) fn dragging(app: &App) -> bool {
    app.mouse.drag.is_some()
        || (app.current_screen == Screen::FileEditor
            && app
                .editor_state
                .as_ref()
                .is_some_and(|editor| editor.mouse_drag_active()))
}

pub(crate) fn tick(app: &mut App) {
    if blocked(app) {
        cancel_gesture(app);
        return;
    }
    if app.current_screen == Screen::FileEditor {
        if let Some(editor) = app.editor_state.as_mut() {
            editor.tick_mouse_drag();
        }
    }
}

pub(crate) fn handle_input(app: &mut App, event: MouseEvent) {
    if blocked(app) {
        cancel_gesture(app);
        return;
    }
    match app.current_screen {
        Screen::FilePanel => handle_panel(app, event),
        Screen::FileEditor => {
            if let Some(editor) = app.editor_state.as_mut() {
                let inside = editor
                    .mouse
                    .area
                    .is_some_and(|area| contains(area, event.column, event.row));
                let captured = editor.mouse_drag_active()
                    && matches!(
                        event.kind,
                        MouseEventKind::Drag(MouseButton::Left)
                            | MouseEventKind::Up(MouseButton::Left)
                    );
                if inside || captured {
                    editor.handle_mouse(event);
                }
            }
        }
        Screen::FileViewer => {
            if let Some(viewer) = app.viewer_state.as_mut() {
                if viewer.search_mode || viewer.goto_mode {
                    return;
                }
                if viewer
                    .mouse_area
                    .is_some_and(|area| contains(area, event.column, event.row))
                {
                    match event.kind {
                        MouseEventKind::ScrollUp => viewer.mouse_scroll(-3),
                        MouseEventKind::ScrollDown => viewer.mouse_scroll(3),
                        MouseEventKind::ScrollLeft => {
                            viewer.horizontal_scroll = viewer.horizontal_scroll.saturating_sub(3)
                        }
                        MouseEventKind::ScrollRight if !viewer.word_wrap => {
                            viewer.horizontal_scroll = viewer.horizontal_scroll.saturating_add(3)
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }
}

fn panel_at(app: &App, event: MouseEvent) -> Option<usize> {
    app.panels.iter().position(|panel| {
        panel
            .mouse_area
            .is_some_and(|area| contains(area, event.column, event.row))
    })
}

fn handle_panel(app: &mut App, event: MouseEvent) {
    let target = panel_at(app, event);
    match event.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let Some(index) = target else { return };
            let panel = &mut app.panels[index];
            let height = panel.mouse_area.map_or(0, |area| area.height as usize);
            let delta = if event.kind == MouseEventKind::ScrollUp {
                -3
            } else {
                3
            };
            panel.scroll_offset = scroll_delta(panel.scroll_offset, delta)
                .min(panel.files.len().saturating_sub(height));
            panel.mouse_scroll = true;
            app.mouse.click = None;
        }
        MouseEventKind::Down(MouseButton::Left) => {
            app.mouse.drag = None;
            let Some(index) = target else {
                app.mouse.click = None;
                return;
            };
            let panel = &mut app.panels[index];
            let Some(area) = panel.mouse_area else { return };
            let row = panel.scroll_offset + event.row.saturating_sub(area.y) as usize;
            app.active_panel_index = index;
            let Some(file) = panel.files.get(row) else {
                panel.selected_files.clear();
                app.mouse.click = None;
                return;
            };
            let name = file.name.clone();
            let previous = panel.selected_index;
            if event.modifiers.contains(KeyModifiers::SHIFT) {
                panel.selected_files.clear();
                for file in panel
                    .files
                    .iter()
                    .take(previous.max(row) + 1)
                    .skip(previous.min(row))
                {
                    if file.name != ".." {
                        panel.selected_files.insert(file.name.clone());
                    }
                }
            } else if event.modifiers.contains(KeyModifiers::CONTROL) {
                if name != ".." && !panel.selected_files.remove(&name) {
                    panel.selected_files.insert(name.clone());
                }
            } else if !panel.selected_files.contains(&name) {
                panel.selected_files.clear();
            }
            panel.selected_index = row;
            let double_click = event.modifiers.is_empty()
                && app.mouse.click.as_ref().is_some_and(|click| {
                    click.panel == panel.mouse_id
                        && click.generation == panel.listing_generation
                        && click.name == name
                        && click.position == (event.column, event.row)
                        && click.time.elapsed() < Duration::from_millis(400)
                });
            app.mouse.click = None;
            if double_click {
                app.enter_selected();
                return;
            }
            let panel_id = panel.mouse_id;
            let generation = panel.listing_generation;
            let clipboard = if name == ".." {
                Err("The parent entry cannot be dragged".into())
            } else {
                app.capture_drag_clipboard()
            };
            app.mouse.drag = Some(FileDrag {
                panel: panel_id,
                generation,
                name,
                position: (event.column, event.row),
                clipboard,
                modifiers: event.modifiers,
                moved: false,
            });
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(drag) = app.mouse.drag.as_mut() {
                drag.moved |= drag.position != (event.column, event.row);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let Some(drag) = app.mouse.drag.take() else {
                return;
            };
            let Some(source) = app.panels.iter().position(|panel| {
                panel.mouse_id == drag.panel && panel.listing_generation == drag.generation
            }) else {
                return;
            };
            if !drag.moved && drag.position == (event.column, event.row) {
                if drag.modifiers.is_empty() && event.modifiers.is_empty() {
                    app.mouse.click = Some(FileClick {
                        panel: drag.panel,
                        generation: drag.generation,
                        name: drag.name,
                        position: drag.position,
                        time: Instant::now(),
                    });
                }
                return;
            }
            let Some(target) = target.filter(|target| *target != source) else {
                return;
            };
            let mut clipboard = match drag.clipboard {
                Ok(clipboard) => clipboard,
                Err(error) => {
                    app.show_message(&error);
                    return;
                }
            };
            clipboard.operation = if event.modifiers.contains(KeyModifiers::SHIFT) {
                ClipboardOperation::Cut
            } else {
                ClipboardOperation::Copy
            };
            app.active_panel_index = target;
            app.clipboard = Some(clipboard);
            // Existing paste owns destination authorization, collision prompts,
            // cancellation, progress, cross-volume moves and remote restrictions.
            app.clipboard_paste();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn wheel_updates_scrollbar_and_preserves_focus_and_cursor_after_redraw() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        for n in 0..25 {
            std::fs::write(right.path().join(format!("{n:02}.txt")), "test").unwrap();
        }
        let mut app = App::new(left.path().into(), right.path().into());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| super::super::draw::draw(frame, &mut app))
            .unwrap();
        let area = app.panels[1].mouse_area.unwrap();
        let scrollbar_cells = |terminal: &Terminal<TestBackend>| {
            (area.y..area.bottom())
                .map(|row| {
                    terminal.backend().buffer()[(area.right(), row)]
                        .symbol()
                        .to_string()
                })
                .collect::<Vec<_>>()
        };
        let top_scrollbar = scrollbar_cells(&terminal);
        handle_input(&mut app, event(MouseEventKind::ScrollDown, area.x, area.y));
        terminal
            .draw(|frame| super::super::draw::draw(frame, &mut app))
            .unwrap();
        assert_eq!(app.active_panel_index, 0);
        assert_eq!(app.panels[1].selected_index, 0);
        assert_eq!(app.panels[1].scroll_offset, 3);
        assert_ne!(scrollbar_cells(&terminal), top_scrollbar);
        handle_input(
            &mut app,
            event(MouseEventKind::ScrollDown, area.x, area.y - 1),
        );
        assert_eq!(app.panels[1].scroll_offset, 3);

        for _ in 0..app.panels[1].files.len() {
            handle_input(&mut app, event(MouseEventKind::ScrollDown, area.x, area.y));
        }
        terminal
            .draw(|frame| super::super::draw::draw(frame, &mut app))
            .unwrap();
        assert_eq!(
            app.panels[1].scroll_offset,
            app.panels[1].files.len() - area.height as usize
        );
        let bottom_scrollbar = scrollbar_cells(&terminal);
        let last_track_cell = top_scrollbar.len() - 2;
        assert_ne!(top_scrollbar[1], top_scrollbar[last_track_cell]);
        assert_ne!(bottom_scrollbar[1], top_scrollbar[1]);
        assert_eq!(bottom_scrollbar[last_track_cell], top_scrollbar[1]);

        for _ in 0..app.panels[1].files.len() {
            handle_input(&mut app, event(MouseEventKind::ScrollUp, area.x, area.y));
        }
        terminal
            .draw(|frame| super::super::draw::draw(frame, &mut app))
            .unwrap();
        assert_eq!(app.panels[1].scroll_offset, 0);
        assert_eq!(scrollbar_cells(&terminal), top_scrollbar);
        assert_eq!(app.active_panel_index, 0);
        assert_eq!(app.panels[1].selected_index, 0);
    }

    #[test]
    fn resize_cancels_a_pending_drop_and_clears_old_hit_rectangles() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        std::fs::write(left.path().join("a.txt"), "a").unwrap();
        let mut app = App::new(left.path().into(), right.path().into());
        app.panels[0].mouse_area = Some(Rect::new(1, 2, 30, 5));
        handle_input(
            &mut app,
            event(MouseEventKind::Down(MouseButton::Left), 2, 3),
        );
        assert!(app.mouse.drag.is_some());
        begin_frame(&mut app, Rect::new(0, 0, 120, 30));
        assert!(app.mouse.drag.is_none());
        assert!(app.panels.iter().all(|panel| panel.mouse_area.is_none()));
    }

    #[test]
    fn drop_uses_existing_conflict_confirmation_and_shift_selects_move() {
        for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
            let left = tempfile::tempdir().unwrap();
            let right = tempfile::tempdir().unwrap();
            std::fs::write(left.path().join("a.txt"), "source").unwrap();
            std::fs::write(right.path().join("a.txt"), "destination").unwrap();
            let mut app = App::new(left.path().into(), right.path().into());
            app.panels[0].mouse_area = Some(Rect::new(1, 2, 30, 5));
            app.panels[1].mouse_area = Some(Rect::new(41, 2, 30, 5));
            handle_input(
                &mut app,
                event(MouseEventKind::Down(MouseButton::Left), 2, 3),
            );
            handle_input(
                &mut app,
                event(MouseEventKind::Drag(MouseButton::Left), 42, 3),
            );
            let mut release = event(MouseEventKind::Up(MouseButton::Left), 42, 3);
            release.modifiers = modifiers;
            handle_input(&mut app, release);
            assert_eq!(app.active_panel_index, 1);
            let conflict = app.conflict_state.as_ref().unwrap();
            let clipboard = conflict.clipboard_backup.as_ref().unwrap();
            assert_eq!(
                clipboard.operation,
                if modifiers.is_empty() {
                    ClipboardOperation::Copy
                } else {
                    ClipboardOperation::Cut
                }
            );
            assert_eq!(clipboard.files, vec!["a.txt"]);
            assert!(clipboard.source_authorizations.contains_key("a.txt"));
            assert!(app.file_operation_progress.is_none());
            assert_eq!(
                std::fs::read_to_string(right.path().join("a.txt")).unwrap(),
                "destination"
            );
            assert_eq!(
                std::fs::read_to_string(left.path().join("a.txt")).unwrap(),
                "source"
            );
        }
    }

    #[test]
    fn replacing_a_panel_in_the_same_slot_does_not_retarget_a_drop() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        std::fs::write(left.path().join("a.txt"), "source").unwrap();
        let mut app = App::new(left.path().into(), right.path().into());
        app.panels[0].mouse_area = Some(Rect::new(1, 2, 30, 5));
        app.panels[1].mouse_area = Some(Rect::new(41, 2, 30, 5));
        handle_input(
            &mut app,
            event(MouseEventKind::Down(MouseButton::Left), 2, 3),
        );
        app.panels[0] = super::super::app::PanelState::new(left.path().into());
        handle_input(
            &mut app,
            event(MouseEventKind::Drag(MouseButton::Left), 42, 3),
        );
        handle_input(
            &mut app,
            event(MouseEventKind::Up(MouseButton::Left), 42, 3),
        );
        assert!(app.clipboard.is_none());
        assert!(!right.path().join("a.txt").exists());
    }

    #[test]
    fn replacing_source_while_dragging_does_not_authorize_the_replacement() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        let source = left.path().join("a.txt");
        std::fs::write(&source, "original").unwrap();
        std::fs::write(right.path().join("a.txt"), "destination").unwrap();
        let mut app = App::new(left.path().into(), right.path().into());
        app.panels[0].mouse_area = Some(Rect::new(1, 2, 30, 5));
        app.panels[1].mouse_area = Some(Rect::new(41, 2, 30, 5));
        handle_input(
            &mut app,
            event(MouseEventKind::Down(MouseButton::Left), 2, 3),
        );
        std::fs::rename(&source, left.path().join("original.txt")).unwrap();
        std::fs::write(&source, "replacement").unwrap();
        handle_input(
            &mut app,
            event(MouseEventKind::Drag(MouseButton::Left), 42, 3),
        );
        handle_input(
            &mut app,
            event(MouseEventKind::Up(MouseButton::Left), 42, 3),
        );
        let clipboard = app
            .conflict_state
            .as_ref()
            .unwrap()
            .clipboard_backup
            .as_ref()
            .unwrap();
        let authorization = clipboard.source_authorizations.get("a.txt").unwrap();
        assert!(crate::services::file_ops::verify_path_authorization(
            &source,
            authorization,
            "Drag source"
        )
        .is_err());
        assert_eq!(
            std::fs::read_to_string(right.path().join("a.txt")).unwrap(),
            "destination"
        );
    }

    #[test]
    fn double_click_opens_the_clicked_directory() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        let directory = left.path().join("folder");
        std::fs::create_dir(&directory).unwrap();
        let mut app = App::new(left.path().into(), right.path().into());
        app.panels[0].mouse_area = Some(Rect::new(1, 2, 30, 5));
        handle_input(
            &mut app,
            event(MouseEventKind::Down(MouseButton::Left), 2, 3),
        );
        handle_input(&mut app, event(MouseEventKind::Up(MouseButton::Left), 2, 3));
        handle_input(
            &mut app,
            event(MouseEventKind::Down(MouseButton::Left), 2, 3),
        );
        assert_eq!(app.panels[0].path, directory);
        assert!(app.mouse.drag.is_none());
    }

    #[test]
    fn control_click_toggles_marks_and_shift_click_selects_a_range() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(left.path().join(name), "test").unwrap();
        }
        let mut app = App::new(left.path().into(), right.path().into());
        app.panels[0].mouse_area = Some(Rect::new(1, 2, 30, 5));
        for row in [3, 5] {
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                let mut click = event(kind, 2, row);
                click.modifiers = KeyModifiers::CONTROL;
                handle_input(&mut app, click);
            }
        }
        assert_eq!(
            app.panels[0].selected_files,
            ["a.txt", "c.txt"]
                .into_iter()
                .map(String::from)
                .collect::<std::collections::HashSet<_>>()
        );
        let mut click = event(MouseEventKind::Down(MouseButton::Left), 2, 4);
        click.modifiers = KeyModifiers::SHIFT;
        handle_input(&mut app, click);
        assert_eq!(app.panels[0].selected_index, 2);
        assert_eq!(
            app.panels[0].selected_files,
            ["b.txt", "c.txt"]
                .into_iter()
                .map(String::from)
                .collect::<std::collections::HashSet<_>>()
        );
        assert!(app.mouse.click.is_none());
        cancel_gesture(&mut app);
    }

    #[test]
    fn a_modal_blocks_background_clicks_and_cancels_drag() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        std::fs::write(left.path().join("a.txt"), "source").unwrap();
        let mut app = App::new(left.path().into(), right.path().into());
        app.panels[0].mouse_area = Some(Rect::new(1, 2, 30, 5));
        app.panels[1].mouse_area = Some(Rect::new(41, 2, 30, 5));
        handle_input(
            &mut app,
            event(MouseEventKind::Down(MouseButton::Left), 2, 3),
        );
        app.show_mkdir_dialog();
        handle_input(
            &mut app,
            event(MouseEventKind::Up(MouseButton::Left), 42, 3),
        );
        assert!(app.mouse.drag.is_none());
        assert_eq!(app.active_panel_index, 0);
        assert!(app.clipboard.is_none());
    }
}
