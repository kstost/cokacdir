mod ui;
mod services;
mod utils;

use std::io;
use std::path::PathBuf;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::time::Duration;
use ratatui::{
    backend::CrosstermBackend,
    Terminal,
};

use crate::ui::app::{App, Screen};

fn main() -> io::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Clear screen before entering alternate screen
    execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
        EnterAlternateScreen,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Create app state
    let left_path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let right_path = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let mut app = App::new(left_path, right_path);

    // Run app
    let result = run_app(&mut terminal, &mut app);

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
        crossterm::cursor::Show
    )?;

    if let Err(err) = result {
        eprintln!("Error: {}", err);
    }

    Ok(())
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui::draw::draw(f, app))?;

        // For AI screen, use polling to allow async response updates
        let poll_timeout = if app.current_screen == Screen::AIScreen {
            Duration::from_millis(100) // Fast polling for spinner animation
        } else {
            Duration::from_millis(250)
        };

        // Poll for AI responses if on AI screen
        if app.current_screen == Screen::AIScreen {
            if let Some(ref mut state) = app.ai_state {
                state.poll_response();
            }
        }

        // Check for key events with timeout
        if event::poll(poll_timeout)? {
            if let Event::Key(key) = event::read()? {
                match app.current_screen {
                    Screen::DualPanel => {
                        if handle_dual_panel_input(app, key.code, key.modifiers) {
                            return Ok(());
                        }
                    }
                    Screen::FileViewer => {
                        ui::file_viewer::handle_input(app, key.code, key.modifiers);
                    }
                    Screen::FileEditor => {
                        ui::file_editor::handle_input(app, key.code, key.modifiers);
                    }
                    Screen::FileInfo => {
                        ui::file_info::handle_input(app, key.code);
                    }
                    Screen::ProcessManager => {
                        ui::process_manager::handle_input(app, key.code);
                    }
                    Screen::Help => {
                        // Any key closes help
                        app.current_screen = Screen::DualPanel;
                    }
                    Screen::AIScreen => {
                        if let Some(ref mut state) = app.ai_state {
                            if ui::ai_screen::handle_input(state, key.code, key.modifiers) {
                                // Save session before leaving
                                app.save_ai_session();
                                app.current_screen = Screen::DualPanel;
                                app.ai_state = None;
                            }
                        }
                    }
                    Screen::SystemInfo => {
                        if ui::system_info::handle_input(&mut app.system_info_state, key.code) {
                            app.current_screen = Screen::DualPanel;
                        }
                    }
                    Screen::ImageViewer => {
                        ui::image_viewer::handle_input(app, key.code);
                    }
                    Screen::SearchResult => {
                        let should_close = ui::search_result::handle_input(
                            &mut app.search_result_state,
                            key.code,
                        );
                        if should_close {
                            if key.code == KeyCode::Enter {
                                // Enter: 선택한 경로로 이동
                                app.goto_search_result();
                            } else {
                                // Esc: 검색 결과 화면 닫기
                                app.search_result_state.active = false;
                                app.current_screen = Screen::DualPanel;
                            }
                        }
                    }
                }
            }
        }
    }
}

fn handle_dual_panel_input(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    // Handle advanced search dialog first
    if app.advanced_search_state.active {
        if let Some(criteria) = ui::advanced_search::handle_input(&mut app.advanced_search_state, code) {
            app.execute_advanced_search(&criteria);
        }
        return false;
    }

    // Handle dialog input first
    if app.dialog.is_some() {
        return ui::dialogs::handle_dialog_input(app, code, modifiers);
    }

    match code {
        // Quit
        KeyCode::Char('0') | KeyCode::Char('q') | KeyCode::Char('Q') => return true,

        // Navigation
        KeyCode::Up => app.move_cursor(-1),
        KeyCode::Down => app.move_cursor(1),
        KeyCode::PageUp => app.move_cursor(-10),
        KeyCode::PageDown => app.move_cursor(10),
        KeyCode::Home => app.cursor_to_start(),
        KeyCode::End => app.cursor_to_end(),

        // Tab - switch panels
        KeyCode::Tab => app.switch_panel(),

        // Left/Right - switch panels (keep same index position)
        KeyCode::Left | KeyCode::Right => app.switch_panel_keep_index(),

        // Enter - open directory or file
        KeyCode::Enter => app.enter_selected(),

        // Escape - go to parent directory
        KeyCode::Esc => app.go_to_parent(),

        // Space - select/deselect file
        KeyCode::Char(' ') => app.toggle_selection(),

        // * - select/deselect all
        KeyCode::Char('*') => app.toggle_all_selection(),

        // Sort keys
        KeyCode::Char('n') | KeyCode::Char('N') => app.toggle_sort_by_name(),
        KeyCode::Char('s') | KeyCode::Char('S') => app.toggle_sort_by_size(),
        KeyCode::Char('d') | KeyCode::Char('D') => app.toggle_sort_by_date(),

        // Function keys (alphabet)
        KeyCode::Char('h') | KeyCode::Char('H') => app.show_help(),
        KeyCode::Char('o') | KeyCode::Char('O') => app.show_file_info(),
        KeyCode::Char('v') | KeyCode::Char('V') => app.view_file(),
        KeyCode::Char('e') | KeyCode::Char('E') => app.edit_file(),
        KeyCode::Char('c') | KeyCode::Char('C') => app.show_copy_dialog(),
        KeyCode::Char('m') | KeyCode::Char('M') => app.show_move_dialog(),
        KeyCode::Char('k') | KeyCode::Char('K') => app.show_mkdir_dialog(),
        KeyCode::Char('x') | KeyCode::Char('X') | KeyCode::Delete | KeyCode::Backspace => app.show_delete_dialog(),
        KeyCode::Char('p') | KeyCode::Char('P') => app.show_process_manager(),
        KeyCode::Char('r') | KeyCode::Char('R') => app.show_rename_dialog(),
        KeyCode::Char('f') => app.show_search_dialog(),
        KeyCode::Char('/') => app.show_goto_dialog(),
        KeyCode::Char('~') => {
            // 홈 폴더로 바로 이동
            if let Some(home) = dirs::home_dir() {
                app.execute_goto(&home.display().to_string());
            }
        }

        // AI screen - '.' or Ctrl+A
        KeyCode::Char('.') => app.show_ai_screen(),
        KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => app.show_ai_screen(),

        // System info - 'i' or 'I'
        KeyCode::Char('i') | KeyCode::Char('I') => app.show_system_info(),

        _ => {}
    }
    false
}
