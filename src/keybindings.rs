use crossterm::event::{KeyCode, KeyModifiers};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::Hash;

// ─── Generic key binding infrastructure ────────────────────────────────

/// A key combination (key code + modifiers).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyBind {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

/// Generic reverse-lookup map: KeyBind → Action.
///
/// This is the reusable core of the keybinding system.
/// Each screen context (FilePanel, Viewer, Editor, …) gets its own `ActionMap`
/// parameterised by its action enum.
pub struct ActionMap<A> {
    map: HashMap<KeyBind, A>,
    display: HashMap<A, Vec<String>>,
}

impl<A: Copy + Eq + Hash> ActionMap<A> {
    /// Build an `ActionMap` by merging user overrides on top of defaults.
    ///
    /// - Actions present in `overrides` completely replace the default bindings
    ///   for that action.
    /// - Actions **not** present in `overrides` keep the default bindings.
    pub fn build(defaults: &HashMap<A, Vec<String>>, overrides: &HashMap<A, Vec<String>>) -> Self {
        let mut merged = defaults.clone();
        for (action, keys) in overrides {
            merged.insert(*action, keys.clone());
        }

        let mut map = HashMap::new();
        for (action, key_strings) in &merged {
            for key_str in key_strings {
                let binds = parse_key(key_str);
                for bind in binds {
                    map.insert(bind, *action);
                }
            }
        }

        // Build forward display map (action → formatted key strings, comments filtered)
        let mut display: HashMap<A, Vec<String>> = HashMap::new();
        for (action, key_strings) in &merged {
            let keys: Vec<String> = key_strings
                .iter()
                .filter(|s| !s.trim().starts_with("//"))
                .map(|s| format_key_display(s))
                .collect();
            display.insert(*action, keys);
        }

        Self { map, display }
    }

    /// Get formatted display strings for an action (e.g. `["Ctrl+C"]`).
    pub fn keys(&self, action: A) -> &[String] {
        self.display
            .get(&action)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Get the first display key for an action (e.g. `"Ctrl+C"`).
    /// Returns empty string if no keys are bound.
    pub fn first_key(&self, action: A) -> &str {
        self.keys(action).first().map(|s| s.as_str()).unwrap_or("")
    }

    /// Get all keys joined with a separator (e.g. `"Ctrl+C / Shift+V"`).
    pub fn keys_joined(&self, action: A, sep: &str) -> String {
        self.keys(action).join(sep)
    }

    /// Look up an action for the given key event.
    pub fn lookup(&self, code: KeyCode, modifiers: KeyModifiers) -> Option<A> {
        // Try exact match first
        let bind = KeyBind { code, modifiers };
        if let Some(action) = self.map.get(&bind) {
            return Some(*action);
        }
        // For Char keys, crossterm may report SHIFT for uppercase letters
        // and shifted symbols (e.g. '*' = Shift+8). Try without SHIFT.
        if let KeyCode::Char(_) = code {
            if modifiers.contains(KeyModifiers::SHIFT) {
                let stripped = modifiers & !KeyModifiers::SHIFT;
                let bind2 = KeyBind {
                    code,
                    modifiers: stripped,
                };
                return self.map.get(&bind2).copied();
            }
        }
        None
    }
}

// ─── Key string parsing (shared by all contexts) ──────────────────────

/// Parse a key string like `"ctrl+shift+c"`, `"shift+up"`, `"q"`, `"f1"`
/// into one or more `KeyBind` values.
///
/// Alphabetic characters always produce **both** lowercase and uppercase
/// variants so that bindings are case-insensitive regardless of modifiers.
pub fn parse_key(s: &str) -> Vec<KeyBind> {
    let trimmed = s.trim();
    // Strings starting with "//" are treated as comments and skipped.
    if trimmed.starts_with("//") {
        return Vec::new();
    }
    let s = trimmed.to_lowercase();
    let parts: Vec<&str> = s.split('+').collect();

    let mut modifiers = KeyModifiers::NONE;
    let key_part;

    if parts.len() == 1 {
        key_part = parts[0];
    } else {
        for &part in &parts[..parts.len() - 1] {
            match part {
                "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
                "shift" => modifiers |= KeyModifiers::SHIFT,
                "alt" => modifiers |= KeyModifiers::ALT,
                _ => {}
            }
        }
        key_part = parts[parts.len() - 1];
    }

    let code = match key_part {
        "up" => Some(KeyCode::Up),
        "down" => Some(KeyCode::Down),
        "left" => Some(KeyCode::Left),
        "right" => Some(KeyCode::Right),
        "enter" | "return" => Some(KeyCode::Enter),
        "esc" | "escape" => Some(KeyCode::Esc),
        "tab" => Some(KeyCode::Tab),
        "space" => Some(KeyCode::Char(' ')),
        "backspace" => Some(KeyCode::Backspace),
        "delete" | "del" => Some(KeyCode::Delete),
        "home" => Some(KeyCode::Home),
        "end" => Some(KeyCode::End),
        "pageup" => Some(KeyCode::PageUp),
        "pagedown" => Some(KeyCode::PageDown),
        "f1" => Some(KeyCode::F(1)),
        "f2" => Some(KeyCode::F(2)),
        "f3" => Some(KeyCode::F(3)),
        "f4" => Some(KeyCode::F(4)),
        "f5" => Some(KeyCode::F(5)),
        "f6" => Some(KeyCode::F(6)),
        "f7" => Some(KeyCode::F(7)),
        "f8" => Some(KeyCode::F(8)),
        "f9" => Some(KeyCode::F(9)),
        "f10" => Some(KeyCode::F(10)),
        "f11" => Some(KeyCode::F(11)),
        "f12" => Some(KeyCode::F(12)),
        s if s.len() == 1 => {
            let ch = s.chars().next().unwrap_or(' ');
            Some(KeyCode::Char(ch))
        }
        _ => None,
    };

    let Some(code) = code else {
        return Vec::new();
    };

    // Alphabetic keys: always register both lowercase and uppercase
    // so bindings are case-insensitive regardless of modifier combination.
    if let KeyCode::Char(ch) = code {
        if ch.is_ascii_alphabetic() {
            let lower = ch.to_ascii_lowercase();
            let upper = ch.to_ascii_uppercase();
            return vec![
                KeyBind {
                    code: KeyCode::Char(lower),
                    modifiers,
                },
                KeyBind {
                    code: KeyCode::Char(upper),
                    modifiers,
                },
            ];
        }
    }

    vec![KeyBind { code, modifiers }]
}

/// Format a key string for user-facing display.
///
/// `"ctrl+shift+c"` → `"Ctrl+Shift+C"`, `"pageup"` → `"PgUp"`,
/// `"space"` → `"Space"`, `"q"` → `"Q"`.
pub fn format_key_display(s: &str) -> String {
    let s = s.trim().to_lowercase();
    let parts: Vec<&str> = s.split('+').collect();

    let mut result = Vec::new();

    for (i, part) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;
        let formatted = if !is_last {
            // Modifier
            match *part {
                "ctrl" | "control" => "Ctrl",
                "shift" => "Shift",
                "alt" => "Alt",
                other => {
                    result.push(other.to_string());
                    continue;
                }
            }
            .to_string()
        } else {
            // Key name
            match *part {
                "up" => "Up".into(),
                "down" => "Down".into(),
                "left" => "Left".into(),
                "right" => "Right".into(),
                "enter" | "return" => "Enter".into(),
                "esc" | "escape" => "Esc".into(),
                "tab" => "Tab".into(),
                "space" => "Space".into(),
                "backspace" => "BkSp".into(),
                "delete" | "del" => "Del".into(),
                "home" => "Home".into(),
                "end" => "End".into(),
                "pageup" => "PgUp".into(),
                "pagedown" => "PgDn".into(),
                "f1" => "F1".into(),
                "f2" => "F2".into(),
                "f3" => "F3".into(),
                "f4" => "F4".into(),
                "f5" => "F5".into(),
                "f6" => "F6".into(),
                "f7" => "F7".into(),
                "f8" => "F8".into(),
                "f9" => "F9".into(),
                "f10" => "F10".into(),
                "f11" => "F11".into(),
                "f12" => "F12".into(),
                s if s.len() == 1 => s.to_uppercase(),
                other => other.to_string(),
            }
        };
        result.push(formatted);
    }

    result.join("+")
}

// ─── FilePanel context ─────────────────────────────────────────────────

/// All possible actions in the FilePanel context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelAction {
    Quit,
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    GoHome,
    GoEnd,
    Open,
    ParentDir,
    SwitchPanel,
    SwitchPanelLeft,
    SwitchPanelRight,
    ToggleSelect,
    SelectAll,
    SelectByExtension,
    SelectUp,
    SelectDown,
    Copy,
    Cut,
    Paste,
    SortByName,
    SortByType,
    SortBySize,
    SortByDate,
    Help,
    FileInfo,
    Edit,
    Mkdir,
    Mkfile,
    Delete,
    ProcessManager,
    Rename,
    Tar,
    Search,
    GoToPath,
    AddPanel,
    GoHomeDir,
    Refresh,
    GitLogDiff,
    StartDiff,
    ClosePanel,
    #[serde(rename = "ai_screen")]
    AIScreen,
    Settings,
    GitScreen,
    ToggleBookmark,
    SetHandler,
    EncryptAll,
    DecryptAll,
    RemoveDuplicates,
    #[cfg(target_os = "macos")]
    OpenInFinder,
    #[cfg(target_os = "macos")]
    OpenInVSCode,
    #[cfg(target_os = "windows")]
    OpenInExplorer,
    #[cfg(target_os = "windows")]
    OpenInVSCode,
}

/// Default keybindings for FilePanel (matches the original hardcoded keys).
pub fn default_panel_keybindings() -> HashMap<PanelAction, Vec<String>> {
    let mut m = HashMap::new();

    // General
    m.insert(PanelAction::Quit, vec!["//Quit program".into(), "q".into()]);
    m.insert(
        PanelAction::Help,
        vec!["//Show help screen".into(), "?".into()],
    );
    m.insert(
        PanelAction::Settings,
        vec!["//Open settings dialog".into(), "`".into()],
    );
    m.insert(
        PanelAction::Refresh,
        vec!["//Refresh panels".into(), "2".into()],
    );

    // Navigation
    m.insert(
        PanelAction::MoveUp,
        vec!["//Move cursor up".into(), "up".into(), "k".into()],
    );
    m.insert(
        PanelAction::MoveDown,
        vec!["//Move cursor down".into(), "down".into(), "j".into()],
    );
    m.insert(
        PanelAction::PageUp,
        vec!["//Page up (10 lines)".into(), "pageup".into()],
    );
    m.insert(
        PanelAction::PageDown,
        vec!["//Page down (10 lines)".into(), "pagedown".into()],
    );
    m.insert(
        PanelAction::GoHome,
        vec!["//Go to first item".into(), "home".into()],
    );
    m.insert(
        PanelAction::GoEnd,
        vec!["//Go to last item".into(), "end".into()],
    );
    m.insert(
        PanelAction::Open,
        vec!["//Open file or enter directory".into(), "enter".into()],
    );
    m.insert(
        PanelAction::ParentDir,
        vec![
            "//Go to parent directory (or cancel diff)".into(),
            "esc".into(),
        ],
    );
    m.insert(
        PanelAction::GoToPath,
        vec!["//Go to path".into(), "/".into()],
    );
    m.insert(
        PanelAction::GoHomeDir,
        vec!["//Go to home directory".into(), "1".into()],
    );

    // Panel
    m.insert(
        PanelAction::SwitchPanel,
        vec!["//Switch to next panel".into(), "tab".into()],
    );
    m.insert(
        PanelAction::SwitchPanelLeft,
        vec!["//Switch to left panel".into(), "left".into(), "h".into()],
    );
    m.insert(
        PanelAction::SwitchPanelRight,
        vec!["//Switch to right panel".into(), "right".into(), "l".into()],
    );
    m.insert(
        PanelAction::AddPanel,
        vec!["//Add new panel".into(), "0".into()],
    );
    m.insert(
        PanelAction::ClosePanel,
        vec!["//Close current panel".into(), "9".into()],
    );

    // Selection
    m.insert(
        PanelAction::ToggleSelect,
        vec!["//Toggle file selection".into(), "space".into()],
    );
    m.insert(
        PanelAction::SelectAll,
        vec!["//Select/deselect all".into(), "*".into(), "ctrl+a".into()],
    );
    m.insert(
        PanelAction::SelectByExtension,
        vec!["//Select by extension".into(), ";".into()],
    );
    m.insert(
        PanelAction::SelectUp,
        vec!["//Select and move up".into(), "shift+up".into()],
    );
    m.insert(
        PanelAction::SelectDown,
        vec!["//Select and move down".into(), "shift+down".into()],
    );

    // Clipboard
    m.insert(
        PanelAction::Copy,
        vec!["//Copy selected files".into(), "ctrl+c".into()],
    );
    m.insert(
        PanelAction::Cut,
        vec!["//Cut selected files".into(), "ctrl+x".into()],
    );
    #[cfg(not(windows))]
    m.insert(
        PanelAction::Paste,
        vec!["//Paste files".into(), "ctrl+v".into(), "shift+v".into()],
    );
    #[cfg(windows)]
    m.insert(
        PanelAction::Paste,
        vec!["//Paste files".into(), "shift+v".into()],
    );

    // Sort
    m.insert(
        PanelAction::SortByName,
        vec!["//Sort by name".into(), "n".into()],
    );
    m.insert(
        PanelAction::SortByType,
        vec!["//Sort by type".into(), "y".into()],
    );
    m.insert(
        PanelAction::SortBySize,
        vec!["//Sort by size".into(), "s".into()],
    );
    m.insert(
        PanelAction::SortByDate,
        vec!["//Sort by date".into(), "d".into()],
    );

    // File operations
    m.insert(
        PanelAction::FileInfo,
        vec!["//Show file info".into(), "i".into()],
    );
    m.insert(PanelAction::Edit, vec!["//Edit file".into(), "e".into()]);
    m.insert(
        PanelAction::Mkdir,
        vec!["//Create directory".into(), ",".into()],
    );
    m.insert(
        PanelAction::Mkfile,
        vec!["//Create file".into(), "m".into()],
    );
    m.insert(
        PanelAction::Delete,
        vec![
            "//Delete file".into(),
            "x".into(),
            "delete".into(),
            "backspace".into(),
        ],
    );
    m.insert(
        PanelAction::Rename,
        vec!["//Rename file".into(), "r".into()],
    );
    m.insert(PanelAction::Tar, vec!["//Archive (tar)".into(), "t".into()]);
    m.insert(
        PanelAction::Search,
        vec!["//Search files".into(), "f".into()],
    );
    m.insert(
        PanelAction::SetHandler,
        vec!["//Set extension handler".into(), "u".into()],
    );

    // Tools
    m.insert(
        PanelAction::ProcessManager,
        vec!["//Process manager".into(), "p".into()],
    );
    m.insert(
        PanelAction::AIScreen,
        vec!["//AI assistant shortcut disabled".into()],
    );
    m.insert(
        PanelAction::ToggleBookmark,
        vec!["//Toggle bookmark".into(), "'".into()],
    );

    // Git / Diff
    m.insert(
        PanelAction::GitScreen,
        vec!["//Git screen".into(), "g".into()],
    );
    m.insert(
        PanelAction::GitLogDiff,
        vec!["//Git log diff".into(), "7".into()],
    );
    m.insert(
        PanelAction::StartDiff,
        vec!["//Start diff".into(), "8".into()],
    );

    // Encryption
    m.insert(
        PanelAction::EncryptAll,
        vec!["//Encrypt all files in directory".into(), "shift+e".into()],
    );
    m.insert(
        PanelAction::DecryptAll,
        vec!["//Decrypt all .cokacenc files".into(), "shift+d".into()],
    );
    m.insert(
        PanelAction::RemoveDuplicates,
        vec!["//Remove duplicate files".into(), "shift+x".into()],
    );

    // macOS only
    #[cfg(target_os = "macos")]
    {
        m.insert(
            PanelAction::OpenInFinder,
            vec!["//Open in Finder".into(), "o".into()],
        );
        m.insert(
            PanelAction::OpenInVSCode,
            vec!["//Open in VS Code".into(), "c".into()],
        );
    }

    // Windows only
    #[cfg(target_os = "windows")]
    {
        m.insert(
            PanelAction::OpenInExplorer,
            vec!["//Open in Explorer".into(), "o".into()],
        );
        m.insert(
            PanelAction::OpenInVSCode,
            vec!["//Open in VS Code".into(), "c".into()],
        );
    }

    m
}

// ─── FileEditor context ────────────────────────────────────────────────

/// All possible actions in the FileEditor context.
///
/// Only Ctrl/Alt modifier combinations and Esc are mapped here.
/// Basic text editing keys (arrows, Home/End, Enter, Tab, Backspace,
/// Delete, character input) remain hardcoded in the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditorAction {
    Save,
    Cut,
    Undo,
    Redo,
    SelectAll,
    Copy,
    Paste,
    ToggleWordWrap,
    DeleteLine,
    DuplicateLine,
    SelectNextOccurrence,
    SelectLine,
    ToggleComment,
    Indent,
    InsertLineBelow,
    InsertLineAbove,
    MoveWordLeft,
    MoveWordRight,
    DeleteWordBackward,
    DeleteWordForward,
    Find,
    Replace,
    GotoLine,
    GoToFileStart,
    GoToFileEnd,
    MoveLineUp,
    MoveLineDown,
    Exit,
}

/// Default keybindings for FileEditor.
pub fn default_editor_keybindings() -> HashMap<EditorAction, Vec<String>> {
    let mut m = HashMap::new();

    // File
    m.insert(
        EditorAction::Save,
        vec!["//Save file".into(), "ctrl+s".into()],
    );

    // Clipboard & selection
    m.insert(
        EditorAction::Cut,
        vec!["//Cut (line if no selection)".into(), "ctrl+x".into()],
    );
    m.insert(
        EditorAction::Copy,
        vec!["//Copy (line if no selection)".into(), "ctrl+c".into()],
    );
    m.insert(EditorAction::Paste, vec!["//Paste".into(), "ctrl+v".into()]);
    m.insert(
        EditorAction::SelectAll,
        vec!["//Select all".into(), "ctrl+a".into()],
    );
    m.insert(
        EditorAction::SelectNextOccurrence,
        vec!["//Select next occurrence".into(), "ctrl+d".into()],
    );
    m.insert(
        EditorAction::SelectLine,
        vec!["//Select line".into(), "ctrl+l".into()],
    );

    // Editing
    m.insert(EditorAction::Undo, vec!["//Undo".into(), "ctrl+z".into()]);
    m.insert(EditorAction::Redo, vec!["//Redo".into(), "ctrl+y".into()]);
    m.insert(
        EditorAction::DeleteLine,
        vec!["//Delete line".into(), "ctrl+k".into()],
    );
    m.insert(
        EditorAction::DuplicateLine,
        vec!["//Duplicate line".into(), "ctrl+j".into()],
    );
    m.insert(
        EditorAction::ToggleComment,
        vec![
            "//Toggle comment".into(),
            "ctrl+/".into(),
            "ctrl+_".into(),
            "ctrl+7".into(),
        ],
    );
    m.insert(
        EditorAction::Indent,
        vec!["//Indent".into(), "ctrl+]".into()],
    );
    m.insert(
        EditorAction::InsertLineBelow,
        vec!["//Insert line below".into(), "ctrl+enter".into()],
    );
    m.insert(
        EditorAction::InsertLineAbove,
        vec!["//Insert line above".into(), "ctrl+shift+enter".into()],
    );

    // Word navigation / deletion
    m.insert(
        EditorAction::MoveWordLeft,
        vec![
            "//Move word left".into(),
            "ctrl+left".into(),
            "ctrl+shift+left".into(),
        ],
    );
    m.insert(
        EditorAction::MoveWordRight,
        vec![
            "//Move word right".into(),
            "ctrl+right".into(),
            "ctrl+shift+right".into(),
        ],
    );
    m.insert(
        EditorAction::DeleteWordBackward,
        vec!["//Delete word backward".into(), "ctrl+backspace".into()],
    );
    m.insert(
        EditorAction::DeleteWordForward,
        vec!["//Delete word forward".into(), "ctrl+delete".into()],
    );

    // View
    m.insert(
        EditorAction::ToggleWordWrap,
        vec!["//Toggle word wrap".into(), "ctrl+w".into()],
    );

    // Search / navigation
    m.insert(EditorAction::Find, vec!["//Find".into(), "ctrl+f".into()]);
    m.insert(
        EditorAction::Replace,
        vec!["//Find and replace".into(), "ctrl+h".into()],
    );
    m.insert(
        EditorAction::GotoLine,
        vec!["//Go to line".into(), "ctrl+g".into()],
    );
    m.insert(
        EditorAction::GoToFileStart,
        vec![
            "//Go to file start".into(),
            "ctrl+home".into(),
            "ctrl+shift+home".into(),
        ],
    );
    m.insert(
        EditorAction::GoToFileEnd,
        vec![
            "//Go to file end".into(),
            "ctrl+end".into(),
            "ctrl+shift+end".into(),
        ],
    );

    // Line move
    m.insert(
        EditorAction::MoveLineUp,
        vec!["//Move line up".into(), "alt+up".into()],
    );
    m.insert(
        EditorAction::MoveLineDown,
        vec!["//Move line down".into(), "alt+down".into()],
    );

    // Exit
    m.insert(
        EditorAction::Exit,
        vec!["//Close editor".into(), "esc".into()],
    );

    m
}

// ─── FileInfo context ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileInfoAction {
    Close,
}

pub fn default_file_info_keybindings() -> HashMap<FileInfoAction, Vec<String>> {
    let mut m = HashMap::new();
    m.insert(
        FileInfoAction::Close,
        vec!["//Close file info".into(), "esc".into()],
    );
    m
}

// ─── SystemInfo context ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemInfoAction {
    Quit,
    SwitchTab,
    MoveUp,
    MoveDown,
}

pub fn default_system_info_keybindings() -> HashMap<SystemInfoAction, Vec<String>> {
    let mut m = HashMap::new();
    m.insert(
        SystemInfoAction::Quit,
        vec!["//Close system info".into(), "esc".into(), "q".into()],
    );
    m.insert(
        SystemInfoAction::SwitchTab,
        vec![
            "//Switch tab".into(),
            "tab".into(),
            "left".into(),
            "right".into(),
        ],
    );
    m.insert(
        SystemInfoAction::MoveUp,
        vec!["//Move up (disk tab)".into(), "up".into()],
    );
    m.insert(
        SystemInfoAction::MoveDown,
        vec!["//Move down (disk tab)".into(), "down".into()],
    );
    m
}

// ─── SearchResult context ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchResultAction {
    Close,
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    GoHome,
    GoEnd,
    Open,
}

pub fn default_search_result_keybindings() -> HashMap<SearchResultAction, Vec<String>> {
    let mut m = HashMap::new();
    m.insert(
        SearchResultAction::Close,
        vec!["//Close search results".into(), "esc".into()],
    );
    m.insert(
        SearchResultAction::MoveUp,
        vec!["//Move up".into(), "up".into(), "k".into()],
    );
    m.insert(
        SearchResultAction::MoveDown,
        vec!["//Move down".into(), "down".into(), "j".into()],
    );
    m.insert(
        SearchResultAction::PageUp,
        vec!["//Page up".into(), "pageup".into()],
    );
    m.insert(
        SearchResultAction::PageDown,
        vec!["//Page down".into(), "pagedown".into()],
    );
    m.insert(
        SearchResultAction::GoHome,
        vec!["//Go to first".into(), "home".into(), "g".into()],
    );
    m.insert(
        SearchResultAction::GoEnd,
        vec!["//Go to last".into(), "end".into(), "shift+g".into()],
    );
    m.insert(
        SearchResultAction::Open,
        vec!["//Open selected result".into(), "enter".into()],
    );
    m
}

// ─── AdvancedSearch context ────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvancedSearchAction {
    Cancel,
    Submit,
    MoveUp,
    MoveDown,
}

pub fn default_advanced_search_keybindings() -> HashMap<AdvancedSearchAction, Vec<String>> {
    let mut m = HashMap::new();
    m.insert(
        AdvancedSearchAction::Cancel,
        vec!["//Cancel search".into(), "esc".into()],
    );
    m.insert(
        AdvancedSearchAction::Submit,
        vec!["//Submit search".into(), "enter".into()],
    );
    m.insert(
        AdvancedSearchAction::MoveUp,
        vec!["//Previous field".into(), "up".into()],
    );
    m.insert(
        AdvancedSearchAction::MoveDown,
        vec!["//Next field".into(), "down".into(), "tab".into()],
    );
    m
}

// ─── DiffFileView context ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffFileViewAction {
    Close,
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    GoHome,
    GoEnd,
    NextChange,
    PrevChange,
}

pub fn default_diff_file_view_keybindings() -> HashMap<DiffFileViewAction, Vec<String>> {
    let mut m = HashMap::new();
    m.insert(
        DiffFileViewAction::Close,
        vec!["//Return to diff screen".into(), "esc".into()],
    );
    m.insert(
        DiffFileViewAction::MoveUp,
        vec!["//Scroll up".into(), "up".into()],
    );
    m.insert(
        DiffFileViewAction::MoveDown,
        vec!["//Scroll down".into(), "down".into()],
    );
    m.insert(
        DiffFileViewAction::PageUp,
        vec!["//Page up".into(), "pageup".into()],
    );
    m.insert(
        DiffFileViewAction::PageDown,
        vec!["//Page down".into(), "pagedown".into()],
    );
    m.insert(
        DiffFileViewAction::GoHome,
        vec!["//Go to start".into(), "home".into()],
    );
    m.insert(
        DiffFileViewAction::GoEnd,
        vec!["//Go to end".into(), "end".into()],
    );
    m.insert(
        DiffFileViewAction::NextChange,
        vec!["//Next change".into(), "n".into()],
    );
    m.insert(
        DiffFileViewAction::PrevChange,
        vec!["//Previous change".into(), "shift+n".into(), "p".into()],
    );
    m
}

// ─── DiffScreen context ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffScreenAction {
    MoveUp,
    MoveDown,
    ExpandDir,
    CollapseDir,
    PageUp,
    PageDown,
    GoHome,
    GoEnd,
    ToggleSelect,
    CycleFilter,
    SortByName,
    SortBySize,
    SortByDate,
    SortByType,
    ExpandAll,
    CollapseAll,
    Open,
    Close,
}

pub fn default_diff_screen_keybindings() -> HashMap<DiffScreenAction, Vec<String>> {
    let mut m = HashMap::new();

    // Navigation
    m.insert(
        DiffScreenAction::MoveUp,
        vec!["//Move cursor up".into(), "up".into(), "k".into()],
    );
    m.insert(
        DiffScreenAction::MoveDown,
        vec!["//Move cursor down".into(), "down".into(), "j".into()],
    );
    m.insert(
        DiffScreenAction::ExpandDir,
        vec!["//Expand directory".into(), "right".into(), "l".into()],
    );
    m.insert(
        DiffScreenAction::CollapseDir,
        vec!["//Collapse directory".into(), "left".into(), "h".into()],
    );
    m.insert(
        DiffScreenAction::PageUp,
        vec!["//Page up".into(), "pageup".into()],
    );
    m.insert(
        DiffScreenAction::PageDown,
        vec!["//Page down".into(), "pagedown".into()],
    );
    m.insert(
        DiffScreenAction::GoHome,
        vec!["//Go to first item".into(), "home".into()],
    );
    m.insert(
        DiffScreenAction::GoEnd,
        vec!["//Go to last item".into(), "end".into()],
    );

    // Selection & filter
    m.insert(
        DiffScreenAction::ToggleSelect,
        vec!["//Toggle selection".into(), "space".into()],
    );
    m.insert(
        DiffScreenAction::CycleFilter,
        vec!["//Cycle filter".into(), "f".into()],
    );

    // Sort
    m.insert(
        DiffScreenAction::SortByName,
        vec!["//Sort by name".into(), "n".into()],
    );
    m.insert(
        DiffScreenAction::SortBySize,
        vec!["//Sort by size".into(), "s".into()],
    );
    m.insert(
        DiffScreenAction::SortByDate,
        vec!["//Sort by date".into(), "d".into()],
    );
    m.insert(
        DiffScreenAction::SortByType,
        vec!["//Sort by type".into(), "y".into()],
    );

    // Expand/Collapse all
    m.insert(
        DiffScreenAction::ExpandAll,
        vec!["//Expand all".into(), "e".into()],
    );
    m.insert(
        DiffScreenAction::CollapseAll,
        vec!["//Collapse all".into(), "c".into()],
    );

    // Actions
    m.insert(
        DiffScreenAction::Open,
        vec!["//View file diff / toggle dir".into(), "enter".into()],
    );
    m.insert(
        DiffScreenAction::Close,
        vec!["//Return to file panel".into(), "esc".into()],
    );

    m
}

// ─── FileViewer context ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewerAction {
    Quit,
    Edit,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
    PageUp,
    PageDown,
    GoTop,
    GoBottom,
    Find,
    ToggleBookmark,
    NextBookmark,
    PrevBookmark,
    ToggleWrap,
    ToggleHex,
    GotoLine,
}

pub fn default_viewer_keybindings() -> HashMap<ViewerAction, Vec<String>> {
    let mut m = HashMap::new();
    m.insert(
        ViewerAction::Quit,
        vec!["//Close viewer".into(), "esc".into(), "ctrl+q".into()],
    );
    m.insert(
        ViewerAction::Edit,
        vec!["//Open in editor".into(), "e".into()],
    );
    m.insert(
        ViewerAction::ScrollUp,
        vec!["//Scroll up".into(), "up".into(), "k".into()],
    );
    m.insert(
        ViewerAction::ScrollDown,
        vec!["//Scroll down".into(), "down".into(), "j".into()],
    );
    m.insert(
        ViewerAction::ScrollLeft,
        vec!["//Scroll left".into(), "left".into()],
    );
    m.insert(
        ViewerAction::ScrollRight,
        vec!["//Scroll right".into(), "right".into()],
    );
    m.insert(
        ViewerAction::PageUp,
        vec!["//Page up".into(), "pageup".into()],
    );
    m.insert(
        ViewerAction::PageDown,
        vec!["//Page down".into(), "pagedown".into()],
    );
    m.insert(
        ViewerAction::GoTop,
        vec!["//Go to top".into(), "home".into()],
    );
    m.insert(
        ViewerAction::GoBottom,
        vec!["//Go to bottom".into(), "end".into(), "shift+g".into()],
    );
    m.insert(
        ViewerAction::Find,
        vec!["//Find text".into(), "ctrl+f".into()],
    );
    m.insert(
        ViewerAction::ToggleBookmark,
        vec!["//Toggle bookmark".into(), "b".into()],
    );
    m.insert(
        ViewerAction::NextBookmark,
        vec!["//Next bookmark".into(), "shift+b".into(), "]".into()],
    );
    m.insert(
        ViewerAction::PrevBookmark,
        vec!["//Previous bookmark".into(), "[".into()],
    );
    m.insert(
        ViewerAction::ToggleWrap,
        vec!["//Toggle word wrap".into(), "w".into()],
    );
    m.insert(
        ViewerAction::ToggleHex,
        vec!["//Toggle hex mode".into(), "h".into(), "shift+h".into()],
    );
    m.insert(
        ViewerAction::GotoLine,
        vec!["//Go to line".into(), "ctrl+g".into(), ":".into()],
    );
    m
}

// ─── ImageViewer context ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageViewerAction {
    Close,
    ZoomIn,
    ZoomOut,
    ResetView,
    PanUp,
    PanDown,
    PanLeft,
    PanRight,
    PrevImage,
    NextImage,
    ToggleSelect,
    Delete,
}

pub fn default_image_viewer_keybindings() -> HashMap<ImageViewerAction, Vec<String>> {
    let mut m = HashMap::new();
    m.insert(
        ImageViewerAction::Close,
        vec!["//Close viewer".into(), "esc".into(), "q".into()],
    );
    m.insert(
        ImageViewerAction::ZoomIn,
        vec!["//Zoom in".into(), "+".into(), "=".into()],
    );
    m.insert(
        ImageViewerAction::ZoomOut,
        vec!["//Zoom out".into(), "-".into(), "_".into()],
    );
    m.insert(
        ImageViewerAction::ResetView,
        vec!["//Reset zoom".into(), "r".into()],
    );
    m.insert(
        ImageViewerAction::PanUp,
        vec!["//Pan up".into(), "up".into()],
    );
    m.insert(
        ImageViewerAction::PanDown,
        vec!["//Pan down".into(), "down".into()],
    );
    m.insert(
        ImageViewerAction::PanLeft,
        vec!["//Pan left".into(), "left".into()],
    );
    m.insert(
        ImageViewerAction::PanRight,
        vec!["//Pan right".into(), "right".into()],
    );
    m.insert(
        ImageViewerAction::PrevImage,
        vec![
            "//Previous image".into(),
            "pageup".into(),
            "shift+up".into(),
        ],
    );
    m.insert(
        ImageViewerAction::NextImage,
        vec![
            "//Next image".into(),
            "pagedown".into(),
            "shift+down".into(),
        ],
    );
    m.insert(
        ImageViewerAction::ToggleSelect,
        vec!["//Select image".into(), "space".into()],
    );
    m.insert(
        ImageViewerAction::Delete,
        vec!["//Delete image".into(), "delete".into(), "backspace".into()],
    );
    m
}

// ─── ProcessManager context ────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessManagerAction {
    Quit,
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    GoHome,
    GoEnd,
    SortByPid,
    SortByCpu,
    SortByMem,
    SortByName,
    Kill,
    ForceKill,
    Refresh,
}

pub fn default_process_manager_keybindings() -> HashMap<ProcessManagerAction, Vec<String>> {
    let mut m = HashMap::new();
    m.insert(
        ProcessManagerAction::Quit,
        vec!["//Close manager".into(), "esc".into(), "q".into()],
    );
    m.insert(
        ProcessManagerAction::MoveUp,
        vec!["//Move up".into(), "up".into()],
    );
    m.insert(
        ProcessManagerAction::MoveDown,
        vec!["//Move down".into(), "down".into()],
    );
    m.insert(
        ProcessManagerAction::PageUp,
        vec!["//Page up".into(), "pageup".into()],
    );
    m.insert(
        ProcessManagerAction::PageDown,
        vec!["//Page down".into(), "pagedown".into()],
    );
    m.insert(
        ProcessManagerAction::GoHome,
        vec!["//Go to first".into(), "home".into()],
    );
    m.insert(
        ProcessManagerAction::GoEnd,
        vec!["//Go to last".into(), "end".into()],
    );
    m.insert(
        ProcessManagerAction::SortByPid,
        vec!["//Sort by PID".into(), "p".into()],
    );
    m.insert(
        ProcessManagerAction::SortByCpu,
        vec!["//Sort by CPU".into(), "c".into()],
    );
    m.insert(
        ProcessManagerAction::SortByMem,
        vec!["//Sort by memory".into(), "m".into()],
    );
    m.insert(
        ProcessManagerAction::SortByName,
        vec!["//Sort by name".into(), "n".into()],
    );
    m.insert(
        ProcessManagerAction::Kill,
        vec!["//Kill process (SIGTERM)".into(), "k".into()],
    );
    m.insert(
        ProcessManagerAction::ForceKill,
        vec!["//Force kill (SIGKILL)".into(), "shift+k".into()],
    );
    m.insert(
        ProcessManagerAction::Refresh,
        vec!["//Refresh list".into(), "r".into()],
    );
    m
}

// ─── AIScreen context ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AIScreenAction {
    Escape,
    Submit,
    InsertNewline,
    Backspace,
    DeleteChar,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    ScrollHistoryUp,
    ScrollHistoryDown,
    PageUp,
    PageDown,
    MoveToLineStart,
    MoveToLineEnd,
    ScrollToTop,
    ScrollToBottom,
    KillLineLeft,
    KillLineRight,
    DeleteWordLeft,
    ClearHistory,
    ToggleFullscreen,
}

pub fn default_ai_screen_keybindings() -> HashMap<AIScreenAction, Vec<String>> {
    let mut m = HashMap::new();

    m.insert(
        AIScreenAction::Escape,
        vec!["//Cancel/clear/exit".into(), "esc".into()],
    );
    m.insert(
        AIScreenAction::Submit,
        vec!["//Submit prompt".into(), "enter".into()],
    );
    m.insert(
        AIScreenAction::InsertNewline,
        vec![
            "//Insert newline".into(),
            "shift+enter".into(),
            "ctrl+enter".into(),
            "alt+enter".into(),
            "ctrl+j".into(),
        ],
    );
    m.insert(
        AIScreenAction::Backspace,
        vec!["//Backspace".into(), "backspace".into()],
    );
    m.insert(
        AIScreenAction::DeleteChar,
        vec!["//Delete character".into(), "delete".into()],
    );
    m.insert(
        AIScreenAction::MoveLeft,
        vec!["//Move cursor left".into(), "left".into()],
    );
    m.insert(
        AIScreenAction::MoveRight,
        vec!["//Move cursor right".into(), "right".into()],
    );
    m.insert(
        AIScreenAction::MoveUp,
        vec!["//Move up / history".into(), "up".into()],
    );
    m.insert(
        AIScreenAction::MoveDown,
        vec!["//Move down / history".into(), "down".into()],
    );
    m.insert(
        AIScreenAction::ScrollHistoryUp,
        vec!["//Scroll history up".into(), "ctrl+up".into()],
    );
    m.insert(
        AIScreenAction::ScrollHistoryDown,
        vec!["//Scroll history down".into(), "ctrl+down".into()],
    );
    m.insert(
        AIScreenAction::PageUp,
        vec!["//Page up".into(), "pageup".into()],
    );
    m.insert(
        AIScreenAction::PageDown,
        vec!["//Page down".into(), "pagedown".into()],
    );
    m.insert(
        AIScreenAction::MoveToLineStart,
        vec![
            "//Move to line start".into(),
            "home".into(),
            "ctrl+a".into(),
        ],
    );
    m.insert(
        AIScreenAction::MoveToLineEnd,
        vec!["//Move to line end".into(), "end".into(), "ctrl+e".into()],
    );
    m.insert(
        AIScreenAction::ScrollToTop,
        vec!["//Scroll to top".into(), "ctrl+home".into()],
    );
    m.insert(
        AIScreenAction::ScrollToBottom,
        vec!["//Scroll to bottom".into(), "ctrl+end".into()],
    );
    m.insert(
        AIScreenAction::KillLineLeft,
        vec!["//Kill line left".into(), "ctrl+u".into()],
    );
    m.insert(
        AIScreenAction::KillLineRight,
        vec!["//Kill line right".into(), "ctrl+k".into()],
    );
    m.insert(
        AIScreenAction::DeleteWordLeft,
        vec!["//Delete word left".into(), "ctrl+w".into()],
    );
    m.insert(
        AIScreenAction::ClearHistory,
        vec!["//Clear conversation".into(), "ctrl+l".into()],
    );
    m.insert(
        AIScreenAction::ToggleFullscreen,
        vec!["//Toggle fullscreen".into(), "ctrl+f".into()],
    );

    m
}

// ─── Goto dialog context ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GotoAction {
    BookmarkDelete,
    BookmarkEdit,
}

pub fn default_goto_keybindings() -> HashMap<GotoAction, Vec<String>> {
    let mut m = HashMap::new();
    m.insert(
        GotoAction::BookmarkDelete,
        vec!["//Delete bookmark or profile".into(), "ctrl+d".into()],
    );
    m.insert(
        GotoAction::BookmarkEdit,
        vec!["//Edit remote profile".into(), "ctrl+e".into()],
    );
    m
}

// ─── JSON config & runtime container ───────────────────────────────────

/// JSON-serializable keybindings configuration.
///
/// Each field corresponds to a screen context. New contexts can be added
/// here with `#[serde(default = "default_…")]` so existing config files
/// keep working.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeybindingsConfig {
    #[serde(default = "default_panel_keybindings")]
    pub file_panel: HashMap<PanelAction, Vec<String>>,
    #[serde(default = "default_editor_keybindings")]
    pub file_editor: HashMap<EditorAction, Vec<String>>,
    #[serde(default = "default_file_info_keybindings")]
    pub file_info: HashMap<FileInfoAction, Vec<String>>,
    #[serde(default = "default_system_info_keybindings")]
    pub system_info: HashMap<SystemInfoAction, Vec<String>>,
    #[serde(default = "default_search_result_keybindings")]
    pub search_result: HashMap<SearchResultAction, Vec<String>>,
    #[serde(default = "default_advanced_search_keybindings")]
    pub advanced_search: HashMap<AdvancedSearchAction, Vec<String>>,
    #[serde(default = "default_diff_file_view_keybindings")]
    pub diff_file_view: HashMap<DiffFileViewAction, Vec<String>>,
    #[serde(default = "default_diff_screen_keybindings")]
    pub diff_screen: HashMap<DiffScreenAction, Vec<String>>,
    #[serde(default = "default_viewer_keybindings")]
    pub file_viewer: HashMap<ViewerAction, Vec<String>>,
    #[serde(default = "default_image_viewer_keybindings")]
    pub image_viewer: HashMap<ImageViewerAction, Vec<String>>,
    #[serde(default = "default_process_manager_keybindings")]
    pub process_manager: HashMap<ProcessManagerAction, Vec<String>>,
    #[serde(default = "default_ai_screen_keybindings")]
    pub ai_screen: HashMap<AIScreenAction, Vec<String>>,
    #[serde(default = "default_goto_keybindings")]
    pub goto: HashMap<GotoAction, Vec<String>>,
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            file_panel: default_panel_keybindings(),
            file_editor: default_editor_keybindings(),
            file_info: default_file_info_keybindings(),
            system_info: default_system_info_keybindings(),
            search_result: default_search_result_keybindings(),
            advanced_search: default_advanced_search_keybindings(),
            diff_file_view: default_diff_file_view_keybindings(),
            diff_screen: default_diff_screen_keybindings(),
            file_viewer: default_viewer_keybindings(),
            image_viewer: default_image_viewer_keybindings(),
            process_manager: default_process_manager_keybindings(),
            ai_screen: default_ai_screen_keybindings(),
            goto: default_goto_keybindings(),
        }
    }
}

/// Runtime keybinding container holding an `ActionMap` per context.
///
/// Adding a new context requires:
/// 1. Define the action enum (e.g. `ViewerAction`)
/// 2. Add a `default_viewer_keybindings()` function
/// 3. Add `pub viewer: HashMap<ViewerAction, Vec<String>>` to `KeybindingsConfig`
/// 4. Add `viewer: ActionMap<ViewerAction>` here
/// 5. Build it in `from_config()` and expose a `viewer_action()` method
pub struct Keybindings {
    panel: ActionMap<PanelAction>,
    editor: ActionMap<EditorAction>,
    file_info: ActionMap<FileInfoAction>,
    system_info: ActionMap<SystemInfoAction>,
    search_result: ActionMap<SearchResultAction>,
    advanced_search: ActionMap<AdvancedSearchAction>,
    diff_file_view: ActionMap<DiffFileViewAction>,
    diff_screen: ActionMap<DiffScreenAction>,
    file_viewer: ActionMap<ViewerAction>,
    image_viewer: ActionMap<ImageViewerAction>,
    process_manager: ActionMap<ProcessManagerAction>,
    ai_screen: ActionMap<AIScreenAction>,
    goto: ActionMap<GotoAction>,
}

impl Keybindings {
    pub fn from_config(config: &KeybindingsConfig) -> Self {
        let mut panel_defaults = default_panel_keybindings();
        let mut panel_overrides = config.file_panel.clone();

        // Keep the AI assistant implementation intact, but do not expose it as
        // a panel shortcut. This also prevents older settings.json files that
        // still contain "." for ai_screen from re-enabling the key.
        panel_defaults.remove(&PanelAction::AIScreen);
        panel_overrides.remove(&PanelAction::AIScreen);

        Self {
            panel: ActionMap::build(&panel_defaults, &panel_overrides),
            editor: ActionMap::build(&default_editor_keybindings(), &config.file_editor),
            file_info: ActionMap::build(&default_file_info_keybindings(), &config.file_info),
            system_info: ActionMap::build(&default_system_info_keybindings(), &config.system_info),
            search_result: ActionMap::build(
                &default_search_result_keybindings(),
                &config.search_result,
            ),
            advanced_search: ActionMap::build(
                &default_advanced_search_keybindings(),
                &config.advanced_search,
            ),
            diff_file_view: ActionMap::build(
                &default_diff_file_view_keybindings(),
                &config.diff_file_view,
            ),
            diff_screen: ActionMap::build(&default_diff_screen_keybindings(), &config.diff_screen),
            file_viewer: ActionMap::build(&default_viewer_keybindings(), &config.file_viewer),
            image_viewer: ActionMap::build(
                &default_image_viewer_keybindings(),
                &config.image_viewer,
            ),
            process_manager: ActionMap::build(
                &default_process_manager_keybindings(),
                &config.process_manager,
            ),
            ai_screen: ActionMap::build(&default_ai_screen_keybindings(), &config.ai_screen),
            goto: ActionMap::build(&default_goto_keybindings(), &config.goto),
        }
    }

    // ── FilePanel ──
    pub fn panel_action(&self, code: KeyCode, modifiers: KeyModifiers) -> Option<PanelAction> {
        self.panel.lookup(code, modifiers)
    }
    pub fn panel_keys(&self, action: PanelAction) -> &[String] {
        self.panel.keys(action)
    }
    pub fn panel_first_key(&self, action: PanelAction) -> &str {
        self.panel.first_key(action)
    }
    pub fn panel_keys_joined(&self, action: PanelAction, sep: &str) -> String {
        self.panel.keys_joined(action, sep)
    }

    // ── FileEditor ──
    pub fn editor_action(&self, code: KeyCode, modifiers: KeyModifiers) -> Option<EditorAction> {
        self.editor.lookup(code, modifiers)
    }
    pub fn editor_keys(&self, action: EditorAction) -> &[String] {
        self.editor.keys(action)
    }
    pub fn editor_first_key(&self, action: EditorAction) -> &str {
        self.editor.first_key(action)
    }
    pub fn editor_keys_joined(&self, action: EditorAction, sep: &str) -> String {
        self.editor.keys_joined(action, sep)
    }

    // ── FileInfo ──
    pub fn file_info_action(
        &self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<FileInfoAction> {
        self.file_info.lookup(code, modifiers)
    }
    pub fn file_info_first_key(&self, action: FileInfoAction) -> &str {
        self.file_info.first_key(action)
    }
    pub fn file_info_keys_joined(&self, action: FileInfoAction, sep: &str) -> String {
        self.file_info.keys_joined(action, sep)
    }

    // ── SystemInfo ──
    pub fn system_info_action(
        &self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<SystemInfoAction> {
        self.system_info.lookup(code, modifiers)
    }
    pub fn system_info_first_key(&self, action: SystemInfoAction) -> &str {
        self.system_info.first_key(action)
    }
    pub fn system_info_keys_joined(&self, action: SystemInfoAction, sep: &str) -> String {
        self.system_info.keys_joined(action, sep)
    }

    // ── SearchResult ──
    pub fn search_result_action(
        &self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<SearchResultAction> {
        self.search_result.lookup(code, modifiers)
    }
    pub fn search_result_first_key(&self, action: SearchResultAction) -> &str {
        self.search_result.first_key(action)
    }
    pub fn search_result_keys_joined(&self, action: SearchResultAction, sep: &str) -> String {
        self.search_result.keys_joined(action, sep)
    }

    // ── AdvancedSearch ──
    pub fn advanced_search_action(
        &self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<AdvancedSearchAction> {
        self.advanced_search.lookup(code, modifiers)
    }
    pub fn advanced_search_first_key(&self, action: AdvancedSearchAction) -> &str {
        self.advanced_search.first_key(action)
    }
    pub fn advanced_search_keys_joined(&self, action: AdvancedSearchAction, sep: &str) -> String {
        self.advanced_search.keys_joined(action, sep)
    }

    // ── DiffFileView ──
    pub fn diff_file_view_action(
        &self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<DiffFileViewAction> {
        self.diff_file_view.lookup(code, modifiers)
    }
    pub fn diff_file_view_first_key(&self, action: DiffFileViewAction) -> &str {
        self.diff_file_view.first_key(action)
    }
    pub fn diff_file_view_keys_joined(&self, action: DiffFileViewAction, sep: &str) -> String {
        self.diff_file_view.keys_joined(action, sep)
    }

    // ── DiffScreen ──
    pub fn diff_screen_action(
        &self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<DiffScreenAction> {
        self.diff_screen.lookup(code, modifiers)
    }
    pub fn diff_screen_first_key(&self, action: DiffScreenAction) -> &str {
        self.diff_screen.first_key(action)
    }
    pub fn diff_screen_keys_joined(&self, action: DiffScreenAction, sep: &str) -> String {
        self.diff_screen.keys_joined(action, sep)
    }

    // ── FileViewer ──
    pub fn viewer_action(&self, code: KeyCode, modifiers: KeyModifiers) -> Option<ViewerAction> {
        self.file_viewer.lookup(code, modifiers)
    }
    pub fn viewer_first_key(&self, action: ViewerAction) -> &str {
        self.file_viewer.first_key(action)
    }
    pub fn viewer_keys_joined(&self, action: ViewerAction, sep: &str) -> String {
        self.file_viewer.keys_joined(action, sep)
    }

    // ── ImageViewer ──
    pub fn image_viewer_action(
        &self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<ImageViewerAction> {
        self.image_viewer.lookup(code, modifiers)
    }
    pub fn image_viewer_first_key(&self, action: ImageViewerAction) -> &str {
        self.image_viewer.first_key(action)
    }
    pub fn image_viewer_keys_joined(&self, action: ImageViewerAction, sep: &str) -> String {
        self.image_viewer.keys_joined(action, sep)
    }

    // ── ProcessManager ──
    pub fn process_manager_action(
        &self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<ProcessManagerAction> {
        self.process_manager.lookup(code, modifiers)
    }
    pub fn process_manager_first_key(&self, action: ProcessManagerAction) -> &str {
        self.process_manager.first_key(action)
    }
    pub fn process_manager_keys_joined(&self, action: ProcessManagerAction, sep: &str) -> String {
        self.process_manager.keys_joined(action, sep)
    }

    // ── AIScreen ──
    pub fn ai_screen_action(
        &self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<AIScreenAction> {
        self.ai_screen.lookup(code, modifiers)
    }
    pub fn ai_screen_first_key(&self, action: AIScreenAction) -> &str {
        self.ai_screen.first_key(action)
    }
    pub fn ai_screen_keys_joined(&self, action: AIScreenAction, sep: &str) -> String {
        self.ai_screen.keys_joined(action, sep)
    }

    // ── Goto dialog ──
    pub fn goto_action(&self, code: KeyCode, modifiers: KeyModifiers) -> Option<GotoAction> {
        self.goto.lookup(code, modifiers)
    }
    pub fn goto_first_key(&self, action: GotoAction) -> &str {
        self.goto.first_key(action)
    }
    pub fn goto_keys_joined(&self, action: GotoAction, sep: &str) -> String {
        self.goto.keys_joined(action, sep)
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_key tests --

    #[test]
    fn test_parse_simple_key() {
        let binds = parse_key("q");
        assert_eq!(binds.len(), 2);
        assert!(binds.contains(&KeyBind {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE
        }));
        assert!(binds.contains(&KeyBind {
            code: KeyCode::Char('Q'),
            modifiers: KeyModifiers::NONE
        }));
    }

    #[test]
    fn test_parse_comment_string() {
        assert!(parse_key("//프로그램 종료").is_empty());
        assert!(parse_key("// quit shortcut").is_empty());
        assert!(parse_key("  // leading spaces  ").is_empty());
    }

    #[test]
    fn test_parse_ctrl_key() {
        let binds = parse_key("ctrl+c");
        assert_eq!(binds.len(), 2);
        assert!(binds.contains(&KeyBind {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL
        }));
        assert!(binds.contains(&KeyBind {
            code: KeyCode::Char('C'),
            modifiers: KeyModifiers::CONTROL
        }));
    }

    #[test]
    fn test_parse_shift_arrow() {
        let binds = parse_key("shift+up");
        assert_eq!(binds.len(), 1);
        assert_eq!(
            binds[0],
            KeyBind {
                code: KeyCode::Up,
                modifiers: KeyModifiers::SHIFT
            }
        );
    }

    #[test]
    fn test_parse_special_keys() {
        assert_eq!(parse_key("enter")[0].code, KeyCode::Enter);
        assert_eq!(parse_key("esc")[0].code, KeyCode::Esc);
        assert_eq!(parse_key("space")[0].code, KeyCode::Char(' '));
        assert_eq!(parse_key("tab")[0].code, KeyCode::Tab);
        assert_eq!(parse_key("backspace")[0].code, KeyCode::Backspace);
        assert_eq!(parse_key("delete")[0].code, KeyCode::Delete);
        assert_eq!(parse_key("pageup")[0].code, KeyCode::PageUp);
        assert_eq!(parse_key("f1")[0].code, KeyCode::F(1));
    }

    #[test]
    fn test_parse_symbol_key() {
        let binds = parse_key("*");
        assert_eq!(binds.len(), 1);
        assert_eq!(
            binds[0],
            KeyBind {
                code: KeyCode::Char('*'),
                modifiers: KeyModifiers::NONE
            }
        );
    }

    #[test]
    fn test_ctrl_shift_letter() {
        let binds = parse_key("ctrl+shift+a");
        assert_eq!(binds.len(), 2);
        assert!(binds.contains(&KeyBind {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        }));
        assert!(binds.contains(&KeyBind {
            code: KeyCode::Char('A'),
            modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        }));
    }

    #[test]
    fn test_ctrl_shift_number() {
        let binds = parse_key("ctrl+shift+1");
        assert_eq!(binds.len(), 1);
        assert_eq!(
            binds[0],
            KeyBind {
                code: KeyCode::Char('1'),
                modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            }
        );
    }

    #[test]
    fn test_alt_ctrl_combination() {
        let binds = parse_key("alt+ctrl+x");
        assert_eq!(binds.len(), 2);
        assert!(binds.contains(&KeyBind {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::ALT | KeyModifiers::CONTROL,
        }));
        assert!(binds.contains(&KeyBind {
            code: KeyCode::Char('X'),
            modifiers: KeyModifiers::ALT | KeyModifiers::CONTROL,
        }));
    }

    // -- format_key_display tests --

    #[test]
    fn test_format_key_display() {
        assert_eq!(format_key_display("q"), "Q");
        assert_eq!(format_key_display("ctrl+c"), "Ctrl+C");
        assert_eq!(format_key_display("shift+up"), "Shift+Up");
        assert_eq!(format_key_display("ctrl+shift+a"), "Ctrl+Shift+A");
        assert_eq!(format_key_display("enter"), "Enter");
        assert_eq!(format_key_display("esc"), "Esc");
        assert_eq!(format_key_display("space"), "Space");
        assert_eq!(format_key_display("pageup"), "PgUp");
        assert_eq!(format_key_display("pagedown"), "PgDn");
        assert_eq!(format_key_display("backspace"), "BkSp");
        assert_eq!(format_key_display("delete"), "Del");
        assert_eq!(format_key_display("f1"), "F1");
        assert_eq!(format_key_display("*"), "*");
        assert_eq!(format_key_display("/"), "/");
    }

    // -- display method tests --

    #[test]
    fn test_action_map_display_keys() {
        let config = KeybindingsConfig::default();
        let kb = Keybindings::from_config(&config);

        // Quit has one key "q" → displayed as "Q"
        assert_eq!(kb.panel_first_key(PanelAction::Quit), "Q");

        // Paste has "ctrl+v" and "shift+v" on Unix, "shift+v" only on Windows
        let paste_keys = kb.panel_keys(PanelAction::Paste);
        #[cfg(not(windows))]
        assert!(paste_keys.contains(&"Ctrl+V".to_string()));
        assert!(paste_keys.contains(&"Shift+V".to_string()));

        // joined
        let joined = kb.panel_keys_joined(PanelAction::SelectAll, "/");
        assert!(joined.contains("*"));
        assert!(joined.contains("Ctrl+A"));
    }

    #[test]
    fn test_display_filters_comments() {
        let config = KeybindingsConfig::default();
        let kb = Keybindings::from_config(&config);

        // Default bindings have "//..." comments — they should not appear in display
        for key_str in kb.panel_keys(PanelAction::Quit) {
            assert!(!key_str.starts_with("//"));
        }
    }

    // -- ActionMap / Keybindings tests --

    #[test]
    fn test_default_keybindings_roundtrip() {
        let config = KeybindingsConfig::default();
        let kb = Keybindings::from_config(&config);

        assert_eq!(
            kb.panel_action(KeyCode::Char('q'), KeyModifiers::NONE),
            Some(PanelAction::Quit)
        );
        assert_eq!(
            kb.panel_action(KeyCode::Char('Q'), KeyModifiers::NONE),
            Some(PanelAction::Quit)
        );
        assert_eq!(
            kb.panel_action(KeyCode::Up, KeyModifiers::NONE),
            Some(PanelAction::MoveUp)
        );
        assert_eq!(
            kb.panel_action(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(PanelAction::Copy)
        );
        assert_eq!(
            kb.panel_action(KeyCode::Up, KeyModifiers::SHIFT),
            Some(PanelAction::SelectUp)
        );
        assert_eq!(
            kb.panel_action(KeyCode::Enter, KeyModifiers::NONE),
            Some(PanelAction::Open)
        );
        assert_eq!(
            kb.panel_action(KeyCode::Char(' '), KeyModifiers::NONE),
            Some(PanelAction::ToggleSelect)
        );
    }

    #[test]
    fn test_shift_char_fallback() {
        let config = KeybindingsConfig::default();
        let kb = Keybindings::from_config(&config);

        assert_eq!(
            kb.panel_action(KeyCode::Char('Q'), KeyModifiers::SHIFT),
            Some(PanelAction::Quit)
        );
        assert_eq!(
            kb.panel_action(KeyCode::Char('*'), KeyModifiers::SHIFT),
            Some(PanelAction::SelectAll)
        );
    }

    #[test]
    fn test_explicit_shift_binding_takes_priority() {
        let config = KeybindingsConfig::default();
        let kb = Keybindings::from_config(&config);

        assert_eq!(
            kb.panel_action(KeyCode::Char('V'), KeyModifiers::SHIFT),
            Some(PanelAction::Paste)
        );
    }

    #[test]
    fn test_unknown_key_returns_none() {
        let config = KeybindingsConfig::default();
        let kb = Keybindings::from_config(&config);
        assert_eq!(kb.panel_action(KeyCode::F(12), KeyModifiers::NONE), None);
    }

    #[test]
    fn test_ai_screen_panel_shortcut_is_disabled() {
        let config = KeybindingsConfig::default();
        let kb = Keybindings::from_config(&config);

        assert_eq!(
            kb.panel_action(KeyCode::Char('.'), KeyModifiers::NONE),
            None
        );
        assert!(kb.panel_first_key(PanelAction::AIScreen).is_empty());
    }

    #[test]
    fn test_ai_screen_panel_shortcut_stays_disabled_with_legacy_config() {
        let json = r#"{"file_panel": {"ai_screen": ["."]}}"#;
        let config: KeybindingsConfig = serde_json::from_str(json).unwrap();
        let kb = Keybindings::from_config(&config);

        assert_eq!(
            kb.panel_action(KeyCode::Char('.'), KeyModifiers::NONE),
            None
        );
        assert!(kb.panel_first_key(PanelAction::AIScreen).is_empty());
    }

    #[test]
    fn test_config_serialization() {
        let config = KeybindingsConfig::default();
        let json = serde_json::to_string_pretty(&config).unwrap();
        let parsed: KeybindingsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.file_panel.len(), config.file_panel.len());
    }

    #[test]
    fn test_partial_config_merges_with_defaults() {
        let json = r#"{"file_panel": {"quit": ["ctrl+q"]}}"#;
        let config: KeybindingsConfig = serde_json::from_str(json).unwrap();
        let kb = Keybindings::from_config(&config);

        assert_eq!(
            kb.panel_action(KeyCode::Char('q'), KeyModifiers::CONTROL),
            Some(PanelAction::Quit)
        );
        assert_eq!(
            kb.panel_action(KeyCode::Char('q'), KeyModifiers::NONE),
            None
        );
        assert_eq!(
            kb.panel_action(KeyCode::Up, KeyModifiers::NONE),
            Some(PanelAction::MoveUp)
        );
        assert_eq!(
            kb.panel_action(KeyCode::Enter, KeyModifiers::NONE),
            Some(PanelAction::Open)
        );
        assert_eq!(
            kb.panel_action(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(PanelAction::Copy)
        );
    }

    #[test]
    fn test_ctrl_shift_action_lookup() {
        let json = r#"{"file_panel": {"quit": ["ctrl+shift+d"]}}"#;
        let config: KeybindingsConfig = serde_json::from_str(json).unwrap();
        let kb = Keybindings::from_config(&config);

        assert_eq!(
            kb.panel_action(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            Some(PanelAction::Quit)
        );
        assert_eq!(
            kb.panel_action(
                KeyCode::Char('D'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            Some(PanelAction::Quit)
        );
        // Plain 'd' must NOT match ctrl+shift+d binding (default SortByDate for 'd' is expected)
        assert_ne!(
            kb.panel_action(KeyCode::Char('d'), KeyModifiers::NONE),
            Some(PanelAction::Quit)
        );
    }

    // -- ActionMap generic reusability test --

    #[test]
    fn test_action_map_with_custom_enum() {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        enum TestAction {
            Save,
            Close,
        }

        let mut defaults: HashMap<TestAction, Vec<String>> = HashMap::new();
        defaults.insert(TestAction::Save, vec!["ctrl+s".into()]);
        defaults.insert(TestAction::Close, vec!["ctrl+w".into()]);

        let overrides = HashMap::new(); // no overrides
        let map = ActionMap::build(&defaults, &overrides);

        assert_eq!(
            map.lookup(KeyCode::Char('s'), KeyModifiers::CONTROL),
            Some(TestAction::Save)
        );
        assert_eq!(
            map.lookup(KeyCode::Char('w'), KeyModifiers::CONTROL),
            Some(TestAction::Close)
        );
        assert_eq!(map.lookup(KeyCode::Char('z'), KeyModifiers::CONTROL), None);
    }

    // -- EditorAction tests --

    #[test]
    fn test_default_editor_keybindings_roundtrip() {
        let config = KeybindingsConfig::default();
        let kb = Keybindings::from_config(&config);

        assert_eq!(
            kb.editor_action(KeyCode::Char('s'), KeyModifiers::CONTROL),
            Some(EditorAction::Save)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Char('z'), KeyModifiers::CONTROL),
            Some(EditorAction::Undo)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Char('y'), KeyModifiers::CONTROL),
            Some(EditorAction::Redo)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(EditorAction::Copy)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Char('v'), KeyModifiers::CONTROL),
            Some(EditorAction::Paste)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Char('x'), KeyModifiers::CONTROL),
            Some(EditorAction::Cut)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Char('f'), KeyModifiers::CONTROL),
            Some(EditorAction::Find)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Char('h'), KeyModifiers::CONTROL),
            Some(EditorAction::Replace)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Char('g'), KeyModifiers::CONTROL),
            Some(EditorAction::GotoLine)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Esc, KeyModifiers::NONE),
            Some(EditorAction::Exit)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Up, KeyModifiers::ALT),
            Some(EditorAction::MoveLineUp)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Down, KeyModifiers::ALT),
            Some(EditorAction::MoveLineDown)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Left, KeyModifiers::CONTROL),
            Some(EditorAction::MoveWordLeft)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Right, KeyModifiers::CONTROL),
            Some(EditorAction::MoveWordRight)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Home, KeyModifiers::CONTROL),
            Some(EditorAction::GoToFileStart)
        );
        assert_eq!(
            kb.editor_action(KeyCode::End, KeyModifiers::CONTROL),
            Some(EditorAction::GoToFileEnd)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Enter, KeyModifiers::CONTROL),
            Some(EditorAction::InsertLineBelow)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            Some(EditorAction::InsertLineAbove)
        );
    }

    #[test]
    fn test_editor_shift_variants_mapped() {
        let config = KeybindingsConfig::default();
        let kb = Keybindings::from_config(&config);

        // Ctrl+Shift+Left/Right should map to MoveWordLeft/Right
        assert_eq!(
            kb.editor_action(KeyCode::Left, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            Some(EditorAction::MoveWordLeft)
        );
        assert_eq!(
            kb.editor_action(KeyCode::Right, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            Some(EditorAction::MoveWordRight)
        );
        // Ctrl+Shift+Home/End should map to GoToFileStart/End
        assert_eq!(
            kb.editor_action(KeyCode::Home, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            Some(EditorAction::GoToFileStart)
        );
        assert_eq!(
            kb.editor_action(KeyCode::End, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            Some(EditorAction::GoToFileEnd)
        );
    }

    #[test]
    fn test_editor_display_keys() {
        let config = KeybindingsConfig::default();
        let kb = Keybindings::from_config(&config);

        assert_eq!(kb.editor_first_key(EditorAction::Save), "Ctrl+S");
        assert_eq!(kb.editor_first_key(EditorAction::Exit), "Esc");

        // ToggleComment has three keys
        let comment_keys = kb.editor_keys(EditorAction::ToggleComment);
        assert!(comment_keys.contains(&"Ctrl+/".to_string()));
    }

    #[test]
    fn test_editor_config_override() {
        let json = r#"{"file_editor": {"save": ["ctrl+shift+s"]}}"#;
        let config: KeybindingsConfig = serde_json::from_str(json).unwrap();
        let kb = Keybindings::from_config(&config);

        // Overridden: Ctrl+Shift+S → Save
        assert_eq!(
            kb.editor_action(
                KeyCode::Char('s'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            Some(EditorAction::Save)
        );
        // Original Ctrl+S should no longer map to Save
        assert_eq!(
            kb.editor_action(KeyCode::Char('s'), KeyModifiers::CONTROL),
            None
        );
        // Other defaults still work
        assert_eq!(
            kb.editor_action(KeyCode::Char('z'), KeyModifiers::CONTROL),
            Some(EditorAction::Undo)
        );
    }

    #[test]
    fn test_editor_config_serialization() {
        let config = KeybindingsConfig::default();
        let json = serde_json::to_string_pretty(&config).unwrap();
        let parsed: KeybindingsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.file_editor.len(), config.file_editor.len());
    }

    #[test]
    fn test_editor_no_conflict_with_plain_keys() {
        let config = KeybindingsConfig::default();
        let kb = Keybindings::from_config(&config);

        // Plain arrow keys should NOT be mapped to any editor action
        assert_eq!(kb.editor_action(KeyCode::Up, KeyModifiers::NONE), None);
        assert_eq!(kb.editor_action(KeyCode::Down, KeyModifiers::NONE), None);
        assert_eq!(kb.editor_action(KeyCode::Left, KeyModifiers::NONE), None);
        assert_eq!(kb.editor_action(KeyCode::Right, KeyModifiers::NONE), None);
        assert_eq!(kb.editor_action(KeyCode::Enter, KeyModifiers::NONE), None);
        assert_eq!(
            kb.editor_action(KeyCode::Backspace, KeyModifiers::NONE),
            None
        );
    }
}
