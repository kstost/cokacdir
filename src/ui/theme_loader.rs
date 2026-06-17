use ratatui::style::Color;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

use super::theme::*;

// ═══════════════════════════════════════════════════════════════════════════════
// JSON 구조체 정의 (색상 인덱스를 u8로 표현)
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
pub struct ThemeJson {
    pub name: String,
    #[serde(default)]
    pub palette: PaletteJson,
    #[serde(default)]
    pub state: StateColorsJson,
    #[serde(default)]
    pub panel: PanelColorsJson,
    #[serde(default)]
    pub header: HeaderColorsJson,
    #[serde(default)]
    pub status_bar: StatusBarColorsJson,
    #[serde(default)]
    pub function_bar: FunctionBarColorsJson,
    #[serde(default)]
    pub message: MessageColorsJson,
    #[serde(default)]
    pub dialog: DialogColorsJson,
    #[serde(default)]
    pub confirm_dialog: ConfirmDialogColorsJson,
    #[serde(default)]
    pub settings: SettingsColorsJson,
    #[serde(default)]
    pub editor: EditorColorsJson,
    #[serde(default)]
    pub syntax: SyntaxColorsJson,
    #[serde(default)]
    pub viewer: ViewerColorsJson,
    #[serde(default)]
    pub process_manager: ProcessManagerColorsJson,
    #[serde(default)]
    pub ai_screen: AIScreenColorsJson,
    #[serde(default)]
    pub system_info: SystemInfoColorsJson,
    #[serde(default)]
    pub search_result: SearchResultColorsJson,
    #[serde(default)]
    pub image_viewer: ImageViewerColorsJson,
    #[serde(default)]
    pub file_info: FileInfoColorsJson,
    #[serde(default)]
    pub help: HelpColorsJson,
    #[serde(default)]
    pub advanced_search: AdvancedSearchColorsJson,
    #[serde(default)]
    pub diff: DiffColorsJson,
    #[serde(default)]
    pub diff_file_view: DiffFileViewColorsJson,
    #[serde(default)]
    pub git_screen: GitScreenColorsJson,
    #[serde(default)]
    pub dedup_screen: DedupScreenColorsJson,
}

#[derive(Debug, Deserialize, Default)]
pub struct PaletteJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_254")]
    pub bg_alt: u8,
    #[serde(default = "default_243")]
    pub fg: u8,
    #[serde(default = "default_251")]
    pub fg_dim: u8,
    #[serde(default = "default_238")]
    pub fg_strong: u8,
    #[serde(default = "default_231")]
    pub fg_inverse: u8,
    #[serde(default = "default_21")]
    pub accent: u8,
    #[serde(default = "default_74")]
    pub shortcut: u8,
    #[serde(default = "default_34")]
    pub positive: u8,
    #[serde(default = "default_198")]
    pub highlight: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct StateColorsJson {
    #[serde(default = "default_34")]
    pub success: u8,
    #[serde(default = "default_198")]
    pub warning: u8,
    #[serde(default = "default_198")]
    pub error: u8,
    #[serde(default = "default_21")]
    pub info: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct PanelColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_251")]
    pub border: u8,
    #[serde(default = "default_238")]
    pub border_active: u8,
    #[serde(default = "default_254")]
    pub header_bg: u8,
    #[serde(default = "default_253")]
    pub header_bg_active: u8,
    #[serde(default = "default_249")]
    pub header_text: u8,
    #[serde(default = "default_242")]
    pub header_text_active: u8,
    #[serde(default = "default_243")]
    pub file_text: u8,
    #[serde(default = "default_67")]
    pub directory_text: u8,
    #[serde(default = "default_37")]
    pub symlink_text: u8,
    #[serde(default = "default_67")]
    pub selected_bg: u8,
    #[serde(default = "default_231")]
    pub selected_text: u8,
    #[serde(default = "default_198")]
    pub marked_text: u8,
    #[serde(default = "default_251")]
    pub size_text: u8,
    #[serde(default = "default_251")]
    pub date_text: u8,
    #[serde(default = "default_67")]
    pub remote_indicator: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct HeaderColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_243")]
    pub text: u8,
    #[serde(default = "default_238")]
    pub title: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct StatusBarColorsJson {
    #[serde(default = "default_253")]
    pub bg: u8,
    #[serde(default = "default_249")]
    pub text: u8,
    #[serde(default = "default_251")]
    pub text_dim: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct FunctionBarColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_243")]
    pub key: u8,
    #[serde(default = "default_251")]
    pub label: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct MessageColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_198")]
    pub text: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct DialogColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_238")]
    pub border: u8,
    #[serde(default = "default_238")]
    pub title: u8,
    #[serde(default = "default_243")]
    pub text: u8,
    #[serde(default = "default_251")]
    pub text_dim: u8,
    #[serde(default = "default_243")]
    pub message_text: u8,
    #[serde(default = "default_243")]
    pub input_text: u8,
    #[serde(default = "default_255")]
    pub input_cursor_fg: u8,
    #[serde(default = "default_238")]
    pub input_cursor_bg: u8,
    #[serde(default = "default_74")]
    pub input_prompt: u8,
    #[serde(default = "default_251")]
    pub button_text: u8,
    #[serde(default = "default_67")]
    pub button_selected_bg: u8,
    #[serde(default = "default_231")]
    pub button_selected_text: u8,
    #[serde(default = "default_255")]
    pub autocomplete_bg: u8,
    #[serde(default = "default_243")]
    pub autocomplete_text: u8,
    #[serde(default = "default_67")]
    pub autocomplete_directory_text: u8,
    #[serde(default = "default_67")]
    pub autocomplete_selected_bg: u8,
    #[serde(default = "default_231")]
    pub autocomplete_selected_text: u8,
    #[serde(default = "default_251")]
    pub autocomplete_scroll_info: u8,
    #[serde(default = "default_251")]
    pub preview_suffix_text: u8,
    #[serde(default = "default_74")]
    pub help_key_text: u8,
    #[serde(default = "default_251")]
    pub help_label_text: u8,
    #[serde(default = "default_251")]
    pub progress_label_text: u8,
    #[serde(default = "default_243")]
    pub progress_value_text: u8,
    #[serde(default = "default_67")]
    pub progress_bar_fill: u8,
    #[serde(default = "default_251")]
    pub progress_bar_empty: u8,
    #[serde(default = "default_243")]
    pub progress_percent_text: u8,
    #[serde(default = "default_198")]
    pub conflict_filename_text: u8,
    #[serde(default = "default_251")]
    pub conflict_count_text: u8,
    #[serde(default = "default_117")]
    pub conflict_shortcut_text: u8,
    #[serde(default = "default_238")]
    pub tar_exclude_title: u8,
    #[serde(default = "default_238")]
    pub tar_exclude_border: u8,
    #[serde(default = "default_255")]
    pub tar_exclude_bg: u8,
    #[serde(default = "default_243")]
    pub tar_exclude_message_text: u8,
    #[serde(default = "default_208")]
    pub tar_exclude_path_text: u8,
    #[serde(default = "default_251")]
    pub tar_exclude_scroll_info: u8,
    #[serde(default = "default_251")]
    pub tar_exclude_button_text: u8,
    #[serde(default = "default_67")]
    pub tar_exclude_button_selected_bg: u8,
    #[serde(default = "default_231")]
    pub tar_exclude_button_selected_text: u8,
    #[serde(default = "default_238")]
    pub git_log_diff_title: u8,
    #[serde(default = "default_238")]
    pub git_log_diff_border: u8,
    #[serde(default = "default_255")]
    pub git_log_diff_bg: u8,
    #[serde(default = "default_243")]
    pub git_log_diff_message_text: u8,
    #[serde(default = "default_243")]
    pub git_log_diff_entry_text: u8,
    #[serde(default = "default_34")]
    pub git_log_diff_selected_text: u8,
    #[serde(default = "default_231")]
    pub git_log_diff_cursor_text: u8,
    #[serde(default = "default_67")]
    pub git_log_diff_cursor_bg: u8,
    #[serde(default = "default_251")]
    pub git_log_diff_button_text: u8,
    #[serde(default = "default_231")]
    pub git_log_diff_button_selected_text: u8,
    #[serde(default = "default_67")]
    pub git_log_diff_button_selected_bg: u8,
    #[serde(default = "default_251")]
    pub git_log_diff_button_disabled_text: u8,
    #[serde(default = "default_251")]
    pub git_log_diff_scroll_info: u8,
    #[serde(default = "default_67")]
    pub remote_bookmark_text: u8,
    #[serde(default = "default_243")]
    pub remote_connect_field_label: u8,
    #[serde(default = "default_238")]
    pub remote_connect_field_value: u8,
    #[serde(default = "default_67")]
    pub remote_connect_field_selected_bg: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct ConfirmDialogColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_238")]
    pub border: u8,
    #[serde(default = "default_238")]
    pub title: u8,
    #[serde(default = "default_243")]
    pub message_text: u8,
    #[serde(default = "default_251")]
    pub button_text: u8,
    #[serde(default = "default_67")]
    pub button_selected_bg: u8,
    #[serde(default = "default_231")]
    pub button_selected_text: u8,
}

#[derive(Debug, Deserialize)]
pub struct SettingsColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_238")]
    pub border: u8,
    #[serde(default = "default_238")]
    pub title: u8,
    #[serde(default = "default_243")]
    pub label_text: u8,
    #[serde(default = "default_74")]
    pub prompt: u8,
    #[serde(default = "default_231")]
    pub value_text: u8,
    #[serde(default = "default_67")]
    pub value_bg: u8,
    #[serde(default = "default_74")]
    pub help_key: u8,
    #[serde(default = "default_251")]
    pub help_text: u8,
}

impl Default for SettingsColorsJson {
    fn default() -> Self {
        Self {
            bg: default_255(),
            border: default_238(),
            title: default_238(),
            label_text: default_243(),
            prompt: default_74(),
            value_text: default_231(),
            value_bg: default_67(),
            help_key: default_74(),
            help_text: default_251(),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct EditorColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_238")]
    pub border: u8,
    #[serde(default = "default_253")]
    pub header_bg: u8,
    #[serde(default = "default_249")]
    pub header_text: u8,
    #[serde(default = "default_251")]
    pub header_info: u8,
    #[serde(default = "default_251")]
    pub line_number: u8,
    #[serde(default = "default_243")]
    pub text: u8,
    #[serde(default = "default_238")]
    pub cursor: u8,
    #[serde(default = "default_67")]
    pub selection_bg: u8,
    #[serde(default = "default_231")]
    pub selection_text: u8,
    #[serde(default = "default_198")]
    pub match_bg: u8,
    #[serde(default = "default_208")]
    pub match_current_bg: u8,
    #[serde(default = "default_74")]
    pub bracket_match: u8,
    #[serde(default = "default_198")]
    pub modified_mark: u8,
    #[serde(default = "default_253")]
    pub footer_bg: u8,
    #[serde(default = "default_74")]
    pub footer_key: u8,
    #[serde(default = "default_251")]
    pub footer_text: u8,
    #[serde(default = "default_243")]
    pub find_input_text: u8,
    #[serde(default = "default_251")]
    pub find_option: u8,
    #[serde(default = "default_74")]
    pub find_option_active: u8,
    #[serde(default = "default_248")]
    pub wrap_indicator: u8,
    #[serde(default = "default_214")]
    pub remote_path_text: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct SyntaxColorsJson {
    #[serde(default = "default_127")]
    pub keyword: u8,
    #[serde(default = "default_37")]
    pub type_name: u8,
    #[serde(default = "default_28")]
    pub string: u8,
    #[serde(default = "default_166")]
    pub number: u8,
    #[serde(default = "default_102")]
    pub comment: u8,
    #[serde(default = "default_241")]
    pub operator: u8,
    #[serde(default = "default_130")]
    pub function: u8,
    #[serde(default = "default_91")]
    pub macro_name: u8,
    #[serde(default = "default_243")]
    pub attribute: u8,
    #[serde(default = "default_236")]
    pub variable: u8,
    #[serde(default = "default_161")]
    pub constant: u8,
    #[serde(default = "default_240")]
    pub bracket: u8,
    #[serde(default = "default_236")]
    pub normal: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct ViewerColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_238")]
    pub border: u8,
    #[serde(default = "default_249")]
    pub header_text: u8,
    #[serde(default = "default_251")]
    pub line_number: u8,
    #[serde(default = "default_243")]
    pub text: u8,
    #[serde(default = "default_21")]
    pub bookmark_indicator: u8,
    #[serde(default = "default_67")]
    pub search_input_text: u8,
    #[serde(default = "default_255")]
    pub search_cursor_fg: u8,
    #[serde(default = "default_67")]
    pub search_cursor_bg: u8,
    #[serde(default = "default_67")]
    pub search_match_current_bg: u8,
    #[serde(default = "default_255")]
    pub search_match_current_fg: u8,
    #[serde(default = "default_243")]
    pub search_match_other_bg: u8,
    #[serde(default = "default_255")]
    pub search_match_other_fg: u8,
    #[serde(default = "default_251")]
    pub search_info: u8,
    #[serde(default = "default_251")]
    pub hex_offset: u8,
    #[serde(default = "default_243")]
    pub hex_bytes: u8,
    #[serde(default = "default_238")]
    pub hex_ascii: u8,
    #[serde(default = "default_248")]
    pub wrap_indicator: u8,
    #[serde(default = "default_74")]
    pub footer_key: u8,
    #[serde(default = "default_251")]
    pub footer_text: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct ProcessManagerColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_238")]
    pub border: u8,
    #[serde(default = "default_249")]
    pub header_text: u8,
    #[serde(default = "default_21")]
    pub column_header: u8,
    #[serde(default = "default_243")]
    pub text: u8,
    #[serde(default = "default_67")]
    pub selected_bg: u8,
    #[serde(default = "default_231")]
    pub selected_text: u8,
    #[serde(default = "default_198")]
    pub cpu_high: u8,
    #[serde(default = "default_198")]
    pub mem_high: u8,
    #[serde(default = "default_198")]
    pub confirm_text: u8,
    #[serde(default = "default_74")]
    pub footer_key: u8,
    #[serde(default = "default_251")]
    pub footer_text: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct AIScreenColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_238")]
    pub history_border: u8,
    #[serde(default = "default_238")]
    pub history_title: u8,
    #[serde(default = "default_251")]
    pub history_placeholder: u8,
    #[serde(default = "default_251")]
    pub history_scroll_info: u8,
    #[serde(default = "default_67")]
    pub user_prefix: u8,
    #[serde(default = "default_74")]
    pub assistant_prefix: u8,
    #[serde(default = "default_198")]
    pub error_prefix: u8,
    #[serde(default = "default_251")]
    pub system_prefix: u8,
    #[serde(default = "default_243")]
    pub message_text: u8,
    #[serde(default = "default_238")]
    pub input_border: u8,
    #[serde(default = "default_74")]
    pub input_prompt: u8,
    #[serde(default = "default_243")]
    pub input_text: u8,
    #[serde(default = "default_255")]
    pub input_cursor_fg: u8,
    #[serde(default = "default_238")]
    pub input_cursor_bg: u8,
    #[serde(default = "default_251")]
    pub input_placeholder: u8,
    #[serde(default = "default_74")]
    pub processing_spinner: u8,
    #[serde(default = "default_251")]
    pub processing_text: u8,
    #[serde(default = "default_198")]
    pub error_text: u8,
    #[serde(default = "default_136")]
    pub tool_use_prefix: u8,
    #[serde(default = "default_67")]
    pub tool_use_name: u8,
    #[serde(default = "default_243")]
    pub tool_use_input: u8,
    #[serde(default = "default_34")]
    pub tool_result_prefix: u8,
    #[serde(default = "default_243")]
    pub tool_result_text: u8,
    #[serde(default = "default_74")]
    pub footer_key: u8,
    #[serde(default = "default_251")]
    pub footer_text: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct SystemInfoColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_238")]
    pub border: u8,
    #[serde(default = "default_34")]
    pub section_title: u8,
    #[serde(default = "default_243")]
    pub label: u8,
    #[serde(default = "default_243")]
    pub value: u8,
    #[serde(default = "default_34")]
    pub bar_fill: u8,
    #[serde(default = "default_251")]
    pub bar_empty: u8,
    #[serde(default = "default_34")]
    pub usage_low: u8,
    #[serde(default = "default_198")]
    pub usage_medium: u8,
    #[serde(default = "default_198")]
    pub usage_high: u8,
    #[serde(default = "default_238")]
    pub tab_active: u8,
    #[serde(default = "default_21")]
    pub disk_header: u8,
    #[serde(default = "default_243")]
    pub disk_text: u8,
    #[serde(default = "default_67")]
    pub selected_bg: u8,
    #[serde(default = "default_231")]
    pub selected_text: u8,
    #[serde(default = "default_74")]
    pub footer_key: u8,
    #[serde(default = "default_251")]
    pub footer_text: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct SearchResultColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_238")]
    pub border: u8,
    #[serde(default = "default_249")]
    pub header_text: u8,
    #[serde(default = "default_21")]
    pub column_header: u8,
    #[serde(default = "default_251")]
    pub column_header_dim: u8,
    #[serde(default = "default_238")]
    pub directory_text: u8,
    #[serde(default = "default_243")]
    pub file_text: u8,
    #[serde(default = "default_67")]
    pub selected_bg: u8,
    #[serde(default = "default_231")]
    pub selected_text: u8,
    #[serde(default = "default_198")]
    pub match_highlight: u8,
    #[serde(default = "default_251")]
    pub path_text: u8,
    #[serde(default = "default_74")]
    pub footer_key: u8,
    #[serde(default = "default_251")]
    pub footer_text: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct ImageViewerColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_238")]
    pub border: u8,
    #[serde(default = "default_238")]
    pub title_text: u8,
    #[serde(default = "default_74")]
    pub loading_spinner: u8,
    #[serde(default = "default_251")]
    pub loading_text: u8,
    #[serde(default = "default_198")]
    pub error_text: u8,
    #[serde(default = "default_251")]
    pub hint_text: u8,
    #[serde(default = "default_74")]
    pub footer_key: u8,
    #[serde(default = "default_251")]
    pub footer_text: u8,
    #[serde(default = "default_251")]
    pub footer_separator: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct FileInfoColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_238")]
    pub border: u8,
    #[serde(default = "default_238")]
    pub title: u8,
    #[serde(default = "default_251")]
    pub label: u8,
    #[serde(default = "default_243")]
    pub value: u8,
    #[serde(default = "default_67")]
    pub value_name: u8,
    #[serde(default = "default_243")]
    pub value_path: u8,
    #[serde(default = "default_243")]
    pub value_type: u8,
    #[serde(default = "default_67")]
    pub value_size: u8,
    #[serde(default = "default_243")]
    pub value_permission: u8,
    #[serde(default = "default_243")]
    pub value_owner: u8,
    #[serde(default = "default_243")]
    pub value_date: u8,
    #[serde(default = "default_74")]
    pub calculating_spinner: u8,
    #[serde(default = "default_74")]
    pub calculating_text: u8,
    #[serde(default = "default_198")]
    pub error_text: u8,
    #[serde(default = "default_251")]
    pub hint_text: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct HelpColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_238")]
    pub border: u8,
    #[serde(default = "default_238")]
    pub title: u8,
    #[serde(default = "default_67")]
    pub section_title: u8,
    #[serde(default = "default_251")]
    pub section_decorator: u8,
    #[serde(default = "default_74")]
    pub key: u8,
    #[serde(default = "default_74")]
    pub key_highlight: u8,
    #[serde(default = "default_243")]
    pub description: u8,
    #[serde(default = "default_251")]
    pub hint_text: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct AdvancedSearchColorsJson {
    #[serde(default = "default_255")]
    pub bg: u8,
    #[serde(default = "default_238")]
    pub border: u8,
    #[serde(default = "default_238")]
    pub title: u8,
    #[serde(default = "default_243")]
    pub label: u8,
    #[serde(default = "default_243")]
    pub input_text: u8,
    #[serde(default = "default_238")]
    pub input_cursor: u8,
    #[serde(default = "default_21")]
    pub field_bracket: u8,
    #[serde(default = "default_34")]
    pub checkbox_checked: u8,
    #[serde(default = "default_251")]
    pub checkbox_unchecked: u8,
    #[serde(default = "default_251")]
    pub button_text: u8,
    #[serde(default = "default_67")]
    pub button_selected_bg: u8,
    #[serde(default = "default_231")]
    pub button_selected_text: u8,
    #[serde(default = "default_74")]
    pub footer_key: u8,
    #[serde(default = "default_251")]
    pub footer_text: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct DiffColorsJson {
    #[serde(default = "default_235")]
    pub bg: u8,
    #[serde(default = "default_245")]
    pub border: u8,
    #[serde(default = "default_252")]
    pub header_text: u8,
    #[serde(default = "default_117")]
    pub header_label: u8,
    #[serde(default = "default_236")]
    pub column_header_bg: u8,
    #[serde(default = "default_252")]
    pub column_header_text: u8,
    #[serde(default = "default_252")]
    pub same_text: u8,
    #[serde(default = "default_220")]
    pub modified_text: u8,
    #[serde(default = "default_58")]
    pub modified_bg: u8,
    #[serde(default = "default_117")]
    pub left_only_text: u8,
    #[serde(default = "default_22")]
    pub left_only_bg: u8,
    #[serde(default = "default_117")]
    pub right_only_text: u8,
    #[serde(default = "default_24")]
    pub right_only_bg: u8,
    #[serde(default = "default_236")]
    pub empty_bg: u8,
    #[serde(default = "default_117")]
    pub dir_same_text: u8,
    #[serde(default = "default_220")]
    pub dir_modified_text: u8,
    #[serde(default = "default_240")]
    pub cursor_bg: u8,
    #[serde(default = "default_255")]
    pub cursor_text: u8,
    #[serde(default = "default_198")]
    pub marked_text: u8,
    #[serde(default = "default_245")]
    pub size_text: u8,
    #[serde(default = "default_245")]
    pub date_text: u8,
    #[serde(default = "default_237")]
    pub status_bar_bg: u8,
    #[serde(default = "default_252")]
    pub status_bar_text: u8,
    #[serde(default = "default_117")]
    pub filter_label: u8,
    #[serde(default = "default_252")]
    pub stats_text: u8,
    #[serde(default = "default_117")]
    pub footer_key: u8,
    #[serde(default = "default_245")]
    pub footer_text: u8,
    #[serde(default = "default_198")]
    pub panel_selected_border: u8,
    #[serde(default = "default_117")]
    pub progress_spinner: u8,
    #[serde(default = "default_117")]
    pub progress_bar_fill: u8,
    #[serde(default = "default_245")]
    pub progress_bar_empty: u8,
    #[serde(default = "default_252")]
    pub progress_percent_text: u8,
    #[serde(default = "default_252")]
    pub progress_value_text: u8,
    #[serde(default = "default_245")]
    pub progress_hint_text: u8,
}

#[derive(Debug, Deserialize, Default)]
pub struct DiffFileViewColorsJson {
    #[serde(default = "default_235")]
    pub bg: u8,
    #[serde(default = "default_252")]
    pub border: u8,
    #[serde(default = "default_252")]
    pub header_text: u8,
    #[serde(default = "default_245")]
    pub line_number: u8,
    #[serde(default = "default_252")]
    pub same_text: u8,
    #[serde(default = "default_220")]
    pub modified_text: u8,
    #[serde(default = "default_58")]
    pub modified_bg: u8,
    #[serde(default = "default_117")]
    pub left_only_text: u8,
    #[serde(default = "default_22")]
    pub left_only_bg: u8,
    #[serde(default = "default_117")]
    pub right_only_text: u8,
    #[serde(default = "default_24")]
    pub right_only_bg: u8,
    #[serde(default = "default_236")]
    pub empty_bg: u8,
    #[serde(default = "default_94")]
    pub inline_change_bg: u8,
    #[serde(default = "default_220")]
    pub inline_change_text: u8,
    #[serde(default = "default_237")]
    pub status_bar_bg: u8,
    #[serde(default = "default_252")]
    pub status_bar_text: u8,
    #[serde(default = "default_117")]
    pub footer_key: u8,
    #[serde(default = "default_245")]
    pub footer_text: u8,
}

#[derive(Debug, Deserialize)]
pub struct GitScreenColorsJson {
    #[serde(default = "default_234")]
    pub bg: u8,
    #[serde(default = "default_102")]
    pub border: u8,
    #[serde(default = "default_108")]
    pub header_branch: u8,
    #[serde(default = "default_188")]
    pub header_path: u8,
    #[serde(default = "default_110")]
    pub tab_active: u8,
    #[serde(default = "default_102")]
    pub tab_inactive: u8,
    #[serde(default = "default_235")]
    pub tab_bar_bg: u8,
    #[serde(default = "default_108")]
    pub file_staged: u8,
    #[serde(default = "default_180")]
    pub file_modified: u8,
    #[serde(default = "default_174")]
    pub file_untracked: u8,
    #[serde(default = "default_167")]
    pub file_deleted: u8,
    #[serde(default = "default_239")]
    pub selected_bg: u8,
    #[serde(default = "default_195")]
    pub selected_text: u8,
    #[serde(default = "default_146")]
    pub footer_key: u8,
    #[serde(default = "default_102")]
    pub footer_text: u8,
    #[serde(default = "default_102")]
    pub commit_input_border: u8,
    #[serde(default = "default_188")]
    pub commit_input_text: u8,
    #[serde(default = "default_146")]
    pub log_hash: u8,
    #[serde(default = "default_188")]
    pub log_message: u8,
    #[serde(default = "default_110")]
    pub log_author: u8,
    #[serde(default = "default_102")]
    pub log_date: u8,
    #[serde(default = "default_108")]
    pub branch_current: u8,
    #[serde(default = "default_188")]
    pub branch_normal: u8,
    #[serde(default = "default_108")]
    pub diff_add: u8,
    #[serde(default = "default_174")]
    pub diff_remove: u8,
    #[serde(default = "default_110")]
    pub diff_header: u8,
}

impl Default for GitScreenColorsJson {
    fn default() -> Self {
        Self {
            bg: 234,
            border: 102,
            header_branch: 108,
            header_path: 188,
            tab_active: 110,
            tab_inactive: 102,
            tab_bar_bg: 235,
            file_staged: 108,
            file_modified: 180,
            file_untracked: 174,
            file_deleted: 167,
            selected_bg: 239,
            selected_text: 195,
            footer_key: 146,
            footer_text: 102,
            commit_input_border: 102,
            commit_input_text: 188,
            log_hash: 146,
            log_message: 188,
            log_author: 110,
            log_date: 102,
            branch_current: 108,
            branch_normal: 188,
            diff_add: 108,
            diff_remove: 174,
            diff_header: 110,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DedupScreenColorsJson {
    #[serde(default = "default_234")]
    pub bg: u8,
    #[serde(default = "default_102")]
    pub border: u8,
    #[serde(default = "default_110")]
    pub title: u8,
    #[serde(default = "default_108")]
    pub phase_text: u8,
    #[serde(default = "default_188")]
    pub stats_text: u8,
    #[serde(default = "default_108")]
    pub progress_bar_fill: u8,
    #[serde(default = "default_236")]
    pub progress_bar_empty: u8,
    #[serde(default = "default_188")]
    pub progress_text: u8,
    #[serde(default = "default_188")]
    pub log_text: u8,
    #[serde(default = "default_144")]
    pub log_text_alt: u8,
    #[serde(default = "default_174")]
    pub log_deleted: u8,
    #[serde(default = "default_167")]
    pub log_error: u8,
    #[serde(default = "default_146")]
    pub footer_key: u8,
    #[serde(default = "default_102")]
    pub footer_text: u8,
}

impl Default for DedupScreenColorsJson {
    fn default() -> Self {
        Self {
            bg: 234,
            border: 102,
            title: 110,
            phase_text: 108,
            stats_text: 188,
            progress_bar_fill: 108,
            progress_bar_empty: 236,
            progress_text: 188,
            log_text: 188,
            log_text_alt: 144,
            log_deleted: 174,
            log_error: 167,
            footer_key: 146,
            footer_text: 102,
        }
    }
}

// 기본값 함수들
fn default_21() -> u8 {
    21
}
fn default_22() -> u8 {
    22
}
fn default_24() -> u8 {
    24
}
fn default_25() -> u8 {
    25
}
fn default_28() -> u8 {
    28
}
fn default_34() -> u8 {
    34
}
fn default_136() -> u8 {
    136
}
fn default_37() -> u8 {
    37
}
fn default_58() -> u8 {
    58
}
fn default_67() -> u8 {
    67
}
fn default_74() -> u8 {
    74
}
fn default_91() -> u8 {
    91
}
fn default_94() -> u8 {
    94
}
fn default_102() -> u8 {
    102
}
fn default_117() -> u8 {
    117
}
fn default_127() -> u8 {
    127
}
fn default_130() -> u8 {
    130
}
fn default_161() -> u8 {
    161
}
fn default_166() -> u8 {
    166
}
fn default_189() -> u8 {
    189
}
fn default_194() -> u8 {
    194
}
fn default_198() -> u8 {
    198
}
fn default_208() -> u8 {
    208
}
fn default_220() -> u8 {
    220
}
fn default_222() -> u8 {
    222
}
fn default_230() -> u8 {
    230
}
fn default_231() -> u8 {
    231
}
fn default_235() -> u8 {
    235
}
fn default_236() -> u8 {
    236
}
fn default_237() -> u8 {
    237
}
fn default_238() -> u8 {
    238
}
fn default_239() -> u8 {
    239
}
fn default_240() -> u8 {
    240
}
fn default_241() -> u8 {
    241
}
fn default_242() -> u8 {
    242
}
fn default_243() -> u8 {
    243
}
fn default_245() -> u8 {
    245
}
fn default_248() -> u8 {
    248
}
fn default_249() -> u8 {
    249
}
fn default_252() -> u8 {
    252
}
fn default_251() -> u8 {
    251
}
fn default_253() -> u8 {
    253
}
fn default_254() -> u8 {
    254
}
fn default_255() -> u8 {
    255
}
fn default_108() -> u8 {
    108
}
fn default_110() -> u8 {
    110
}
fn default_167() -> u8 {
    167
}
fn default_174() -> u8 {
    174
}
fn default_180() -> u8 {
    180
}
fn default_188() -> u8 {
    188
}
fn default_195() -> u8 {
    195
}
fn default_144() -> u8 {
    144
}
fn default_146() -> u8 {
    146
}
fn default_214() -> u8 {
    214
}
fn default_234() -> u8 {
    234
}

// ═══════════════════════════════════════════════════════════════════════════════
// 테마 로딩 함수
// ═══════════════════════════════════════════════════════════════════════════════

/// 테마 디렉토리 경로 (~/.cokacdir/themes)
pub fn themes_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".cokacdir").join("themes"))
}

/// 테마 파일 경로 (~/.cokacdir/themes/{name}.json)
/// Security: Validates theme name to prevent path traversal attacks
pub fn theme_path(name: &str) -> Option<PathBuf> {
    // Prevent path traversal attacks
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return None;
    }

    // Validate filename
    if name.is_empty() || name.len() > 64 {
        return None;
    }

    // Reject names with control characters
    if name.chars().any(|c| c.is_control()) {
        return None;
    }

    let themes = themes_dir()?;
    let path = themes.join(format!("{}.json", name));

    // Verify the path stays within themes directory after canonicalization
    if let (Ok(canonical_path), Ok(canonical_themes)) = (
        path.canonicalize()
            .map(crate::utils::format::strip_unc_prefix),
        themes
            .canonicalize()
            .map(crate::utils::format::strip_unc_prefix),
    ) {
        if canonical_path.starts_with(&canonical_themes) {
            return Some(path);
        }
        return None;
    }

    // For new files (canonicalize fails), verify parent is themes dir
    if let Some(parent) = path.parent() {
        if parent == themes {
            return Some(path);
        }
    }

    None
}

/// JSON 파일에서 테마 로드
pub fn load_theme(name: &str) -> Option<Theme> {
    let path = theme_path(name)?;
    load_theme_from_path(&path)
}

/// 지정된 경로에서 테마 로드
pub fn load_theme_from_path(path: &PathBuf) -> Option<Theme> {
    let content = fs::read_to_string(path).ok()?;
    let json: ThemeJson = serde_json::from_str(&content).ok()?;
    Some(theme_from_json(&json))
}

/// 색상 인덱스를 Color::Indexed로 변환
fn idx(n: u8) -> Color {
    Color::Indexed(n)
}

/// JSON에서 Theme 생성
pub fn theme_from_json(json: &ThemeJson) -> Theme {
    let palette = Palette {
        bg: idx(json.palette.bg),
        bg_alt: idx(json.palette.bg_alt),
        fg: idx(json.palette.fg),
        fg_dim: idx(json.palette.fg_dim),
        fg_strong: idx(json.palette.fg_strong),
        fg_inverse: idx(json.palette.fg_inverse),
        accent: idx(json.palette.accent),
        shortcut: idx(json.palette.shortcut),
        positive: idx(json.palette.positive),
        highlight: idx(json.palette.highlight),
    };

    let state = StateColors {
        success: idx(json.state.success),
        warning: idx(json.state.warning),
        error: idx(json.state.error),
        info: idx(json.state.info),
    };

    let panel = PanelColors {
        bg: idx(json.panel.bg),
        border: idx(json.panel.border),
        border_active: idx(json.panel.border_active),
        header_bg: idx(json.panel.header_bg),
        header_bg_active: idx(json.panel.header_bg_active),
        header_text: idx(json.panel.header_text),
        header_text_active: idx(json.panel.header_text_active),
        file_text: idx(json.panel.file_text),
        directory_text: idx(json.panel.directory_text),
        symlink_text: idx(json.panel.symlink_text),
        selected_bg: idx(json.panel.selected_bg),
        selected_text: idx(json.panel.selected_text),
        marked_text: idx(json.panel.marked_text),
        size_text: idx(json.panel.size_text),
        date_text: idx(json.panel.date_text),
        remote_indicator: idx(json.panel.remote_indicator),
    };

    let header = HeaderColors {
        bg: idx(json.header.bg),
        text: idx(json.header.text),
        title: idx(json.header.title),
    };

    let status_bar = StatusBarColors {
        bg: idx(json.status_bar.bg),
        text: idx(json.status_bar.text),
        text_dim: idx(json.status_bar.text_dim),
    };

    let function_bar = FunctionBarColors {
        bg: idx(json.function_bar.bg),
        key: idx(json.function_bar.key),
        label: idx(json.function_bar.label),
    };

    let message = MessageColors {
        bg: idx(json.message.bg),
        text: idx(json.message.text),
    };

    let dialog = DialogColors {
        bg: idx(json.dialog.bg),
        border: idx(json.dialog.border),
        title: idx(json.dialog.title),
        text: idx(json.dialog.text),
        text_dim: idx(json.dialog.text_dim),
        message_text: idx(json.dialog.message_text),
        input_text: idx(json.dialog.input_text),
        input_cursor_fg: idx(json.dialog.input_cursor_fg),
        input_cursor_bg: idx(json.dialog.input_cursor_bg),
        input_prompt: idx(json.dialog.input_prompt),
        button_text: idx(json.dialog.button_text),
        button_selected_bg: idx(json.dialog.button_selected_bg),
        button_selected_text: idx(json.dialog.button_selected_text),
        autocomplete_bg: idx(json.dialog.autocomplete_bg),
        autocomplete_text: idx(json.dialog.autocomplete_text),
        autocomplete_directory_text: idx(json.dialog.autocomplete_directory_text),
        autocomplete_selected_bg: idx(json.dialog.autocomplete_selected_bg),
        autocomplete_selected_text: idx(json.dialog.autocomplete_selected_text),
        autocomplete_scroll_info: idx(json.dialog.autocomplete_scroll_info),
        preview_suffix_text: idx(json.dialog.preview_suffix_text),
        help_key_text: idx(json.dialog.help_key_text),
        help_label_text: idx(json.dialog.help_label_text),
        progress_label_text: idx(json.dialog.progress_label_text),
        progress_value_text: idx(json.dialog.progress_value_text),
        progress_bar_fill: idx(json.dialog.progress_bar_fill),
        progress_bar_empty: idx(json.dialog.progress_bar_empty),
        progress_percent_text: idx(json.dialog.progress_percent_text),
        conflict_filename_text: idx(json.dialog.conflict_filename_text),
        conflict_count_text: idx(json.dialog.conflict_count_text),
        conflict_shortcut_text: idx(json.dialog.conflict_shortcut_text),
        tar_exclude_title: idx(json.dialog.tar_exclude_title),
        tar_exclude_border: idx(json.dialog.tar_exclude_border),
        tar_exclude_bg: idx(json.dialog.tar_exclude_bg),
        tar_exclude_message_text: idx(json.dialog.tar_exclude_message_text),
        tar_exclude_path_text: idx(json.dialog.tar_exclude_path_text),
        tar_exclude_scroll_info: idx(json.dialog.tar_exclude_scroll_info),
        tar_exclude_button_text: idx(json.dialog.tar_exclude_button_text),
        tar_exclude_button_selected_bg: idx(json.dialog.tar_exclude_button_selected_bg),
        tar_exclude_button_selected_text: idx(json.dialog.tar_exclude_button_selected_text),
        git_log_diff_title: idx(json.dialog.git_log_diff_title),
        git_log_diff_border: idx(json.dialog.git_log_diff_border),
        git_log_diff_bg: idx(json.dialog.git_log_diff_bg),
        git_log_diff_message_text: idx(json.dialog.git_log_diff_message_text),
        git_log_diff_entry_text: idx(json.dialog.git_log_diff_entry_text),
        git_log_diff_selected_text: idx(json.dialog.git_log_diff_selected_text),
        git_log_diff_cursor_text: idx(json.dialog.git_log_diff_cursor_text),
        git_log_diff_cursor_bg: idx(json.dialog.git_log_diff_cursor_bg),
        git_log_diff_button_text: idx(json.dialog.git_log_diff_button_text),
        git_log_diff_button_selected_text: idx(json.dialog.git_log_diff_button_selected_text),
        git_log_diff_button_selected_bg: idx(json.dialog.git_log_diff_button_selected_bg),
        git_log_diff_button_disabled_text: idx(json.dialog.git_log_diff_button_disabled_text),
        git_log_diff_scroll_info: idx(json.dialog.git_log_diff_scroll_info),
        remote_bookmark_text: idx(json.dialog.remote_bookmark_text),
        remote_connect_field_label: idx(json.dialog.remote_connect_field_label),
        remote_connect_field_value: idx(json.dialog.remote_connect_field_value),
        remote_connect_field_selected_bg: idx(json.dialog.remote_connect_field_selected_bg),
    };

    let confirm_dialog = ConfirmDialogColors {
        bg: idx(json.confirm_dialog.bg),
        border: idx(json.confirm_dialog.border),
        title: idx(json.confirm_dialog.title),
        message_text: idx(json.confirm_dialog.message_text),
        button_text: idx(json.confirm_dialog.button_text),
        button_selected_bg: idx(json.confirm_dialog.button_selected_bg),
        button_selected_text: idx(json.confirm_dialog.button_selected_text),
    };

    let settings = SettingsColors {
        bg: idx(json.settings.bg),
        border: idx(json.settings.border),
        title: idx(json.settings.title),
        label_text: idx(json.settings.label_text),
        prompt: idx(json.settings.prompt),
        value_text: idx(json.settings.value_text),
        value_bg: idx(json.settings.value_bg),
        help_key: idx(json.settings.help_key),
        help_text: idx(json.settings.help_text),
    };

    let editor = EditorColors {
        bg: idx(json.editor.bg),
        border: idx(json.editor.border),
        header_bg: idx(json.editor.header_bg),
        header_text: idx(json.editor.header_text),
        header_info: idx(json.editor.header_info),
        line_number: idx(json.editor.line_number),
        text: idx(json.editor.text),
        cursor: idx(json.editor.cursor),
        selection_bg: idx(json.editor.selection_bg),
        selection_text: idx(json.editor.selection_text),
        match_bg: idx(json.editor.match_bg),
        match_current_bg: idx(json.editor.match_current_bg),
        bracket_match: idx(json.editor.bracket_match),
        modified_mark: idx(json.editor.modified_mark),
        footer_bg: idx(json.editor.footer_bg),
        footer_key: idx(json.editor.footer_key),
        footer_text: idx(json.editor.footer_text),
        find_input_text: idx(json.editor.find_input_text),
        find_option: idx(json.editor.find_option),
        find_option_active: idx(json.editor.find_option_active),
        wrap_indicator: idx(json.editor.wrap_indicator),
        remote_path_text: idx(json.editor.remote_path_text),
    };

    let syntax = SyntaxColors {
        keyword: idx(json.syntax.keyword),
        type_name: idx(json.syntax.type_name),
        string: idx(json.syntax.string),
        number: idx(json.syntax.number),
        comment: idx(json.syntax.comment),
        operator: idx(json.syntax.operator),
        function: idx(json.syntax.function),
        macro_name: idx(json.syntax.macro_name),
        attribute: idx(json.syntax.attribute),
        variable: idx(json.syntax.variable),
        constant: idx(json.syntax.constant),
        bracket: idx(json.syntax.bracket),
        normal: idx(json.syntax.normal),
    };

    let viewer = ViewerColors {
        bg: idx(json.viewer.bg),
        border: idx(json.viewer.border),
        header_text: idx(json.viewer.header_text),
        line_number: idx(json.viewer.line_number),
        text: idx(json.viewer.text),
        bookmark_indicator: idx(json.viewer.bookmark_indicator),
        search_input_text: idx(json.viewer.search_input_text),
        search_cursor_fg: idx(json.viewer.search_cursor_fg),
        search_cursor_bg: idx(json.viewer.search_cursor_bg),
        search_match_current_bg: idx(json.viewer.search_match_current_bg),
        search_match_current_fg: idx(json.viewer.search_match_current_fg),
        search_match_other_bg: idx(json.viewer.search_match_other_bg),
        search_match_other_fg: idx(json.viewer.search_match_other_fg),
        search_info: idx(json.viewer.search_info),
        hex_offset: idx(json.viewer.hex_offset),
        hex_bytes: idx(json.viewer.hex_bytes),
        hex_ascii: idx(json.viewer.hex_ascii),
        wrap_indicator: idx(json.viewer.wrap_indicator),
        footer_key: idx(json.viewer.footer_key),
        footer_text: idx(json.viewer.footer_text),
    };

    let process_manager = ProcessManagerColors {
        bg: idx(json.process_manager.bg),
        border: idx(json.process_manager.border),
        header_text: idx(json.process_manager.header_text),
        column_header: idx(json.process_manager.column_header),
        text: idx(json.process_manager.text),
        selected_bg: idx(json.process_manager.selected_bg),
        selected_text: idx(json.process_manager.selected_text),
        cpu_high: idx(json.process_manager.cpu_high),
        mem_high: idx(json.process_manager.mem_high),
        confirm_text: idx(json.process_manager.confirm_text),
        footer_key: idx(json.process_manager.footer_key),
        footer_text: idx(json.process_manager.footer_text),
    };

    let ai_screen = AIScreenColors {
        bg: idx(json.ai_screen.bg),
        history_border: idx(json.ai_screen.history_border),
        history_title: idx(json.ai_screen.history_title),
        history_placeholder: idx(json.ai_screen.history_placeholder),
        history_scroll_info: idx(json.ai_screen.history_scroll_info),
        user_prefix: idx(json.ai_screen.user_prefix),
        assistant_prefix: idx(json.ai_screen.assistant_prefix),
        error_prefix: idx(json.ai_screen.error_prefix),
        system_prefix: idx(json.ai_screen.system_prefix),
        message_text: idx(json.ai_screen.message_text),
        input_border: idx(json.ai_screen.input_border),
        input_prompt: idx(json.ai_screen.input_prompt),
        input_text: idx(json.ai_screen.input_text),
        input_cursor_fg: idx(json.ai_screen.input_cursor_fg),
        input_cursor_bg: idx(json.ai_screen.input_cursor_bg),
        input_placeholder: idx(json.ai_screen.input_placeholder),
        processing_spinner: idx(json.ai_screen.processing_spinner),
        processing_text: idx(json.ai_screen.processing_text),
        error_text: idx(json.ai_screen.error_text),
        tool_use_prefix: idx(json.ai_screen.tool_use_prefix),
        tool_use_name: idx(json.ai_screen.tool_use_name),
        tool_use_input: idx(json.ai_screen.tool_use_input),
        tool_result_prefix: idx(json.ai_screen.tool_result_prefix),
        tool_result_text: idx(json.ai_screen.tool_result_text),
        footer_key: idx(json.ai_screen.footer_key),
        footer_text: idx(json.ai_screen.footer_text),
    };

    let system_info = SystemInfoColors {
        bg: idx(json.system_info.bg),
        border: idx(json.system_info.border),
        section_title: idx(json.system_info.section_title),
        label: idx(json.system_info.label),
        value: idx(json.system_info.value),
        bar_fill: idx(json.system_info.bar_fill),
        bar_empty: idx(json.system_info.bar_empty),
        usage_low: idx(json.system_info.usage_low),
        usage_medium: idx(json.system_info.usage_medium),
        usage_high: idx(json.system_info.usage_high),
        tab_active: idx(json.system_info.tab_active),
        disk_header: idx(json.system_info.disk_header),
        disk_text: idx(json.system_info.disk_text),
        selected_bg: idx(json.system_info.selected_bg),
        selected_text: idx(json.system_info.selected_text),
        footer_key: idx(json.system_info.footer_key),
        footer_text: idx(json.system_info.footer_text),
    };

    let search_result = SearchResultColors {
        bg: idx(json.search_result.bg),
        border: idx(json.search_result.border),
        header_text: idx(json.search_result.header_text),
        column_header: idx(json.search_result.column_header),
        column_header_dim: idx(json.search_result.column_header_dim),
        directory_text: idx(json.search_result.directory_text),
        file_text: idx(json.search_result.file_text),
        selected_bg: idx(json.search_result.selected_bg),
        selected_text: idx(json.search_result.selected_text),
        match_highlight: idx(json.search_result.match_highlight),
        path_text: idx(json.search_result.path_text),
        footer_key: idx(json.search_result.footer_key),
        footer_text: idx(json.search_result.footer_text),
    };

    let image_viewer = ImageViewerColors {
        bg: idx(json.image_viewer.bg),
        border: idx(json.image_viewer.border),
        title_text: idx(json.image_viewer.title_text),
        loading_spinner: idx(json.image_viewer.loading_spinner),
        loading_text: idx(json.image_viewer.loading_text),
        error_text: idx(json.image_viewer.error_text),
        hint_text: idx(json.image_viewer.hint_text),
        footer_key: idx(json.image_viewer.footer_key),
        footer_text: idx(json.image_viewer.footer_text),
        footer_separator: idx(json.image_viewer.footer_separator),
    };

    let file_info = FileInfoColors {
        bg: idx(json.file_info.bg),
        border: idx(json.file_info.border),
        title: idx(json.file_info.title),
        label: idx(json.file_info.label),
        value: idx(json.file_info.value),
        value_name: idx(json.file_info.value_name),
        value_path: idx(json.file_info.value_path),
        value_type: idx(json.file_info.value_type),
        value_size: idx(json.file_info.value_size),
        value_permission: idx(json.file_info.value_permission),
        value_owner: idx(json.file_info.value_owner),
        value_date: idx(json.file_info.value_date),
        calculating_spinner: idx(json.file_info.calculating_spinner),
        calculating_text: idx(json.file_info.calculating_text),
        error_text: idx(json.file_info.error_text),
        hint_text: idx(json.file_info.hint_text),
    };

    let help = HelpColors {
        bg: idx(json.help.bg),
        border: idx(json.help.border),
        title: idx(json.help.title),
        section_title: idx(json.help.section_title),
        section_decorator: idx(json.help.section_decorator),
        key: idx(json.help.key),
        key_highlight: idx(json.help.key_highlight),
        description: idx(json.help.description),
        hint_text: idx(json.help.hint_text),
    };

    let advanced_search = AdvancedSearchColors {
        bg: idx(json.advanced_search.bg),
        border: idx(json.advanced_search.border),
        title: idx(json.advanced_search.title),
        label: idx(json.advanced_search.label),
        input_text: idx(json.advanced_search.input_text),
        input_cursor: idx(json.advanced_search.input_cursor),
        field_bracket: idx(json.advanced_search.field_bracket),
        checkbox_checked: idx(json.advanced_search.checkbox_checked),
        checkbox_unchecked: idx(json.advanced_search.checkbox_unchecked),
        button_text: idx(json.advanced_search.button_text),
        button_selected_bg: idx(json.advanced_search.button_selected_bg),
        button_selected_text: idx(json.advanced_search.button_selected_text),
        footer_key: idx(json.advanced_search.footer_key),
        footer_text: idx(json.advanced_search.footer_text),
    };

    let diff = DiffColors {
        bg: idx(json.diff.bg),
        border: idx(json.diff.border),
        header_text: idx(json.diff.header_text),
        header_label: idx(json.diff.header_label),
        column_header_bg: idx(json.diff.column_header_bg),
        column_header_text: idx(json.diff.column_header_text),
        same_text: idx(json.diff.same_text),
        modified_text: idx(json.diff.modified_text),
        modified_bg: idx(json.diff.modified_bg),
        left_only_text: idx(json.diff.left_only_text),
        left_only_bg: idx(json.diff.left_only_bg),
        right_only_text: idx(json.diff.right_only_text),
        right_only_bg: idx(json.diff.right_only_bg),
        empty_bg: idx(json.diff.empty_bg),
        dir_same_text: idx(json.diff.dir_same_text),
        dir_modified_text: idx(json.diff.dir_modified_text),
        cursor_bg: idx(json.diff.cursor_bg),
        cursor_text: idx(json.diff.cursor_text),
        marked_text: idx(json.diff.marked_text),
        size_text: idx(json.diff.size_text),
        date_text: idx(json.diff.date_text),
        status_bar_bg: idx(json.diff.status_bar_bg),
        status_bar_text: idx(json.diff.status_bar_text),
        filter_label: idx(json.diff.filter_label),
        stats_text: idx(json.diff.stats_text),
        footer_key: idx(json.diff.footer_key),
        footer_text: idx(json.diff.footer_text),
        panel_selected_border: idx(json.diff.panel_selected_border),
        progress_spinner: idx(json.diff.progress_spinner),
        progress_bar_fill: idx(json.diff.progress_bar_fill),
        progress_bar_empty: idx(json.diff.progress_bar_empty),
        progress_percent_text: idx(json.diff.progress_percent_text),
        progress_value_text: idx(json.diff.progress_value_text),
        progress_hint_text: idx(json.diff.progress_hint_text),
    };

    let diff_file_view = DiffFileViewColors {
        bg: idx(json.diff_file_view.bg),
        border: idx(json.diff_file_view.border),
        header_text: idx(json.diff_file_view.header_text),
        line_number: idx(json.diff_file_view.line_number),
        same_text: idx(json.diff_file_view.same_text),
        modified_text: idx(json.diff_file_view.modified_text),
        modified_bg: idx(json.diff_file_view.modified_bg),
        left_only_text: idx(json.diff_file_view.left_only_text),
        left_only_bg: idx(json.diff_file_view.left_only_bg),
        right_only_text: idx(json.diff_file_view.right_only_text),
        right_only_bg: idx(json.diff_file_view.right_only_bg),
        empty_bg: idx(json.diff_file_view.empty_bg),
        inline_change_bg: idx(json.diff_file_view.inline_change_bg),
        inline_change_text: idx(json.diff_file_view.inline_change_text),
        status_bar_bg: idx(json.diff_file_view.status_bar_bg),
        status_bar_text: idx(json.diff_file_view.status_bar_text),
        footer_key: idx(json.diff_file_view.footer_key),
        footer_text: idx(json.diff_file_view.footer_text),
    };

    let git_screen = GitScreenColors {
        bg: idx(json.git_screen.bg),
        border: idx(json.git_screen.border),
        header_branch: idx(json.git_screen.header_branch),
        header_path: idx(json.git_screen.header_path),
        tab_active: idx(json.git_screen.tab_active),
        tab_inactive: idx(json.git_screen.tab_inactive),
        tab_bar_bg: idx(json.git_screen.tab_bar_bg),
        file_staged: idx(json.git_screen.file_staged),
        file_modified: idx(json.git_screen.file_modified),
        file_untracked: idx(json.git_screen.file_untracked),
        file_deleted: idx(json.git_screen.file_deleted),
        selected_bg: idx(json.git_screen.selected_bg),
        selected_text: idx(json.git_screen.selected_text),
        footer_key: idx(json.git_screen.footer_key),
        footer_text: idx(json.git_screen.footer_text),
        commit_input_border: idx(json.git_screen.commit_input_border),
        commit_input_text: idx(json.git_screen.commit_input_text),
        log_hash: idx(json.git_screen.log_hash),
        log_message: idx(json.git_screen.log_message),
        log_author: idx(json.git_screen.log_author),
        log_date: idx(json.git_screen.log_date),
        branch_current: idx(json.git_screen.branch_current),
        branch_normal: idx(json.git_screen.branch_normal),
        diff_add: idx(json.git_screen.diff_add),
        diff_remove: idx(json.git_screen.diff_remove),
        diff_header: idx(json.git_screen.diff_header),
    };

    let dedup_screen = DedupScreenColors {
        bg: idx(json.dedup_screen.bg),
        border: idx(json.dedup_screen.border),
        title: idx(json.dedup_screen.title),
        phase_text: idx(json.dedup_screen.phase_text),
        stats_text: idx(json.dedup_screen.stats_text),
        progress_bar_fill: idx(json.dedup_screen.progress_bar_fill),
        progress_bar_empty: idx(json.dedup_screen.progress_bar_empty),
        progress_text: idx(json.dedup_screen.progress_text),
        log_text: idx(json.dedup_screen.log_text),
        log_text_alt: idx(json.dedup_screen.log_text_alt),
        log_deleted: idx(json.dedup_screen.log_deleted),
        log_error: idx(json.dedup_screen.log_error),
        footer_key: idx(json.dedup_screen.footer_key),
        footer_text: idx(json.dedup_screen.footer_text),
    };

    Theme {
        palette,
        state,
        panel,
        header,
        status_bar,
        function_bar,
        message,
        dialog,
        confirm_dialog,
        settings,
        editor,
        syntax,
        viewer,
        process_manager,
        ai_screen,
        system_info,
        search_result,
        image_viewer,
        file_info,
        help,
        advanced_search,
        diff,
        diff_file_view,
        git_screen,
        dedup_screen,
        chars: ThemeChars::default(),
    }
}
