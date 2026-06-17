use ratatui::style::{Color, Modifier, Style};
use supports_color::Stream;

/// Default theme name used throughout the application
pub const DEFAULT_THEME_NAME: &str = "dawn_of_coding";

// ═══════════════════════════════════════════════════════════════════════════════
// 아이콘 문자
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct ThemeChars {
    pub folder: char,
    pub file: char,
    pub symlink: char,
    pub folder_open: char,
    pub parent: char,
}

impl Default for ThemeChars {
    fn default() -> Self {
        Self {
            folder: ' ',
            file: ' ',
            symlink: ' ',
            folder_open: ' ',
            parent: ' ',
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 기본 팔레트 (실제 색상값 정의)
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct Palette {
    // 명도 기반 (배경/텍스트용)
    pub bg: Color,         // 기본 배경
    pub bg_alt: Color,     // 대체 배경 (헤더, 상태바)
    pub fg: Color,         // 기본 텍스트
    pub fg_dim: Color,     // 흐린 텍스트 (보조 정보)
    pub fg_strong: Color,  // 강조 텍스트 (디렉토리, 제목)
    pub fg_inverse: Color, // 반전 텍스트 (선택된 항목)

    // 용도 기반 (강조색)
    pub accent: Color,    // 정보성 강조 (컬럼 헤더, 프롬프트)
    pub shortcut: Color,  // 단축키 표시
    pub positive: Color,  // 긍정/성공 (AI 응답, 체크, 진행바)
    pub highlight: Color, // 강조/경고/에러 (통합)
}

// ═══════════════════════════════════════════════════════════════════════════════
// 상태 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct StateColors {
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub info: Color,
}

// ═══════════════════════════════════════════════════════════════════════════════
// 패널 색상 (파일 목록)
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct PanelColors {
    pub bg: Color,
    pub border: Color,
    pub border_active: Color,
    pub header_bg: Color,
    pub header_bg_active: Color,
    pub header_text: Color,
    pub header_text_active: Color, // 활성 패널 헤더 텍스트
    pub file_text: Color,
    pub directory_text: Color,
    pub symlink_text: Color,
    pub selected_bg: Color,
    pub selected_text: Color,
    pub marked_text: Color,
    pub size_text: Color,
    pub date_text: Color,
    pub remote_indicator: Color, // [SSH] 인디케이터 색상
}

// ═══════════════════════════════════════════════════════════════════════════════
// 앱 헤더 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct HeaderColors {
    pub bg: Color,
    pub text: Color,
    pub title: Color,
}

// ═══════════════════════════════════════════════════════════════════════════════
// 상태 표시줄 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct StatusBarColors {
    pub bg: Color,
    pub text: Color,
    pub text_dim: Color,
}

// ═══════════════════════════════════════════════════════════════════════════════
// 함수 바 색상 (하단 단축키 표시줄)
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct FunctionBarColors {
    pub bg: Color,
    pub key: Color,
    pub label: Color,
}

// ═══════════════════════════════════════════════════════════════════════════════
// 메시지 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct MessageColors {
    pub bg: Color,
    pub text: Color,
}

// ═══════════════════════════════════════════════════════════════════════════════
// 대화 상자 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct DialogColors {
    // === 다이얼로그 프레임 ===
    pub bg: Color,     // 배경
    pub border: Color, // 테두리
    pub title: Color,  // 제목

    // === 일반 텍스트 ===
    pub text: Color,         // 일반 텍스트
    pub text_dim: Color,     // 흐린 텍스트
    pub message_text: Color, // 메시지 내용

    // === 입력 필드 ===
    pub input_text: Color,      // 입력 텍스트
    pub input_cursor_fg: Color, // 커서 전경색
    pub input_cursor_bg: Color, // 커서 배경색
    pub input_prompt: Color,    // 프롬프트 ">"

    // === 버튼 (확인 다이얼로그) ===
    pub button_text: Color,          // 일반 버튼 텍스트
    pub button_selected_bg: Color,   // 선택 버튼 배경
    pub button_selected_text: Color, // 선택 버튼 텍스트

    // === 자동완성 목록 ===
    pub autocomplete_bg: Color,             // 목록 배경
    pub autocomplete_text: Color,           // 파일 텍스트
    pub autocomplete_directory_text: Color, // 디렉토리 텍스트
    pub autocomplete_selected_bg: Color,    // 선택 항목 배경
    pub autocomplete_selected_text: Color,  // 선택 항목 텍스트
    pub autocomplete_scroll_info: Color,    // 스크롤 정보 "[1/10]"
    pub preview_suffix_text: Color,         // 미리보기 접미사

    // === 도움말 라인 ===
    pub help_key_text: Color,   // 단축키 텍스트
    pub help_label_text: Color, // 설명 텍스트

    // === 진행률 다이얼로그 ===
    pub progress_label_text: Color,   // "File:", "Total:" 레이블
    pub progress_value_text: Color,   // 파일명, 수치
    pub progress_bar_fill: Color,     // 진행바 채움
    pub progress_bar_empty: Color,    // 진행바 빈 부분
    pub progress_percent_text: Color, // "45%"

    // === 충돌 다이얼로그 ===
    pub conflict_filename_text: Color, // 강조된 파일명
    pub conflict_count_text: Color,    // "(1 of 3 conflicts)"
    pub conflict_shortcut_text: Color, // 버튼 단축키 문자 (O, S, A, l)

    // === Tar 제외 확인 다이얼로그 ===
    pub tar_exclude_title: Color,                // 제목
    pub tar_exclude_border: Color,               // 테두리
    pub tar_exclude_bg: Color,                   // 배경
    pub tar_exclude_message_text: Color,         // 메시지 텍스트
    pub tar_exclude_path_text: Color,            // 제외 경로 텍스트
    pub tar_exclude_scroll_info: Color,          // 스크롤 정보 "[1-5/10]"
    pub tar_exclude_button_text: Color,          // 버튼 텍스트
    pub tar_exclude_button_selected_bg: Color,   // 선택된 버튼 배경
    pub tar_exclude_button_selected_text: Color, // 선택된 버튼 텍스트

    // === Git Log Diff 다이얼로그 ===
    pub git_log_diff_title: Color,                // 제목
    pub git_log_diff_border: Color,               // 테두리
    pub git_log_diff_bg: Color,                   // 배경
    pub git_log_diff_message_text: Color,         // 안내 메시지
    pub git_log_diff_entry_text: Color,           // 커밋 항목 텍스트
    pub git_log_diff_selected_text: Color,        // 선택된(체크) 커밋 텍스트
    pub git_log_diff_cursor_text: Color,          // 커서 위치 텍스트
    pub git_log_diff_cursor_bg: Color,            // 커서 위치 배경
    pub git_log_diff_button_text: Color,          // 비선택 버튼 텍스트
    pub git_log_diff_button_selected_text: Color, // 선택 버튼 텍스트
    pub git_log_diff_button_selected_bg: Color,   // 선택 버튼 배경
    pub git_log_diff_button_disabled_text: Color, // 비활성 버튼 텍스트
    pub git_log_diff_scroll_info: Color,          // 스크롤 정보

    // === 원격 연결 다이얼로그 ===
    pub remote_bookmark_text: Color, // 북마크 목록 내 원격 항목 텍스트
    pub remote_connect_field_label: Color, // 원격 연결 다이얼로그 필드 레이블
    pub remote_connect_field_value: Color, // 원격 연결 다이얼로그 필드 값
    pub remote_connect_field_selected_bg: Color, // 선택된 필드 배경
}

// ═══════════════════════════════════════════════════════════════════════════════
// 확인 다이얼로그 색상 (Large File/Image Confirm)
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct ConfirmDialogColors {
    pub bg: Color,                   // 배경
    pub border: Color,               // 테두리
    pub title: Color,                // 제목
    pub message_text: Color,         // 메시지 텍스트
    pub button_text: Color,          // 일반 버튼 텍스트
    pub button_selected_bg: Color,   // 선택 버튼 배경
    pub button_selected_text: Color, // 선택 버튼 텍스트
}

// ═══════════════════════════════════════════════════════════════════════════════
// 설정 다이얼로그 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct SettingsColors {
    pub bg: Color,         // 배경
    pub border: Color,     // 테두리
    pub title: Color,      // 제목
    pub label_text: Color, // "Theme:" 라벨
    pub prompt: Color,     // ">" 프롬프트
    pub value_text: Color, // 선택된 값 텍스트
    pub value_bg: Color,   // 선택된 값 배경
    pub help_key: Color,   // 단축키
    pub help_text: Color,  // 단축키 설명
}

// ═══════════════════════════════════════════════════════════════════════════════
// 파일 에디터 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct EditorColors {
    pub bg: Color,
    pub border: Color,
    pub header_bg: Color,
    pub header_text: Color,
    pub header_info: Color,
    pub line_number: Color,
    pub text: Color,
    pub cursor: Color,
    pub selection_bg: Color,
    pub selection_text: Color,
    pub match_bg: Color,
    pub match_current_bg: Color,
    pub bracket_match: Color,
    pub modified_mark: Color,
    pub footer_bg: Color,
    pub footer_key: Color,
    pub footer_text: Color,
    pub find_input_text: Color,
    pub find_option: Color,
    pub find_option_active: Color,
    pub wrap_indicator: Color,
    pub remote_path_text: Color,
}

// ═══════════════════════════════════════════════════════════════════════════════
// 코드 하이라이팅 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug)]
pub struct SyntaxColors {
    pub keyword: Color,    // if, else, for, while, fn, let, etc.
    pub type_name: Color,  // String, i32, Vec, etc.
    pub string: Color,     // "hello", 'c'
    pub number: Color,     // 123, 3.14
    pub comment: Color,    // // comment, /* comment */
    pub operator: Color,   // +, -, *, /, =, ==, etc.
    pub function: Color,   // function names
    pub macro_name: Color, // println!, vec!, etc.
    pub attribute: Color,  // #[derive], @decorator
    pub variable: Color,   // variable names
    pub constant: Color,   // CONST_VALUE, true, false
    pub bracket: Color,    // (), [], {}
    pub normal: Color,     // default text
}

// ═══════════════════════════════════════════════════════════════════════════════
// 파일 뷰어 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct ViewerColors {
    pub bg: Color,
    pub border: Color,
    pub header_text: Color,
    pub line_number: Color,
    pub text: Color,
    pub bookmark_indicator: Color, // 북마크 줄 표시 색상
    pub search_input_text: Color,
    pub search_cursor_fg: Color,
    pub search_cursor_bg: Color,
    pub search_match_current_bg: Color,
    pub search_match_current_fg: Color,
    pub search_match_other_bg: Color,
    pub search_match_other_fg: Color,
    pub search_info: Color,
    pub hex_offset: Color,
    pub hex_bytes: Color,
    pub hex_ascii: Color,
    pub wrap_indicator: Color,
    pub footer_key: Color,
    pub footer_text: Color,
}

// ═══════════════════════════════════════════════════════════════════════════════
// 프로세스 관리자 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct ProcessManagerColors {
    pub bg: Color,
    pub border: Color,
    pub header_text: Color,
    pub column_header: Color,
    pub text: Color,
    pub selected_bg: Color,
    pub selected_text: Color,
    pub cpu_high: Color,
    pub mem_high: Color,
    pub confirm_text: Color,
    pub footer_key: Color,
    pub footer_text: Color,
}

// ═══════════════════════════════════════════════════════════════════════════════
// AI 화면 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct AIScreenColors {
    // === 배경 ===
    pub bg: Color,

    // === 히스토리 영역 ===
    pub history_border: Color,      // 히스토리 영역 테두리
    pub history_title: Color,       // 히스토리 제목 (경로 + 세션)
    pub history_placeholder: Color, // 빈 상태 플레이스홀더
    pub history_scroll_info: Color, // 스크롤 정보 "[1/10]"

    // === 메시지 프리픽스 (아이콘) ===
    pub user_prefix: Color,      // "> " 사용자 메시지
    pub assistant_prefix: Color, // "< " AI 응답
    pub error_prefix: Color,     // "! " 에러
    pub system_prefix: Color,    // "* " 시스템

    // === 메시지 내용 ===
    pub message_text: Color, // 일반 메시지 텍스트

    // === 입력 영역 ===
    pub input_border: Color,      // 입력 영역 테두리
    pub input_prompt: Color,      // "> " 입력 프롬프트
    pub input_text: Color,        // 입력 텍스트
    pub input_cursor_fg: Color,   // 커서 전경색 (커서 위 문자)
    pub input_cursor_bg: Color,   // 커서 배경색
    pub input_placeholder: Color, // 플레이스홀더

    // === 처리 중 상태 ===
    pub processing_spinner: Color, // 스피너
    pub processing_text: Color,    // "Processing..." 텍스트

    // === 에러 상태 ===
    pub error_text: Color, // "Claude CLI not available"

    // === 도구 사용 표시 ===
    pub tool_use_prefix: Color,    // "[]" 도구 사용 브래킷
    pub tool_use_name: Color,      // 도구 이름 (Bash, Write 등)
    pub tool_use_input: Color,     // 도구 입력 내용
    pub tool_result_prefix: Color, // "->" 도구 결과 프리픽스
    pub tool_result_text: Color,   // 도구 결과 텍스트

    // === 하단 도움말 ===
    pub footer_key: Color,  // 단축키 텍스트
    pub footer_text: Color, // 설명 텍스트
}

// ═══════════════════════════════════════════════════════════════════════════════
// 시스템 정보 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct SystemInfoColors {
    pub bg: Color,
    pub border: Color,
    pub section_title: Color,
    pub label: Color,
    pub value: Color,
    pub bar_fill: Color,
    pub bar_empty: Color,
    pub usage_low: Color,    // 낮은 사용량 (< 70%)
    pub usage_medium: Color, // 중간 사용량 (70-90%)
    pub usage_high: Color,   // 높은 사용량 (>= 90%)
    pub tab_active: Color,   // 활성 탭 색상
    pub disk_header: Color,
    pub disk_text: Color,
    pub selected_bg: Color,
    pub selected_text: Color,
    pub footer_key: Color,
    pub footer_text: Color,
}

// ═══════════════════════════════════════════════════════════════════════════════
// 검색 결과 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct SearchResultColors {
    pub bg: Color,
    pub border: Color,
    pub header_text: Color,
    pub column_header: Color,
    pub column_header_dim: Color, // 흐린 컬럼 헤더
    pub directory_text: Color,
    pub file_text: Color,
    pub selected_bg: Color,
    pub selected_text: Color,
    pub match_highlight: Color,
    pub path_text: Color,
    pub footer_key: Color,
    pub footer_text: Color,
}

// ═══════════════════════════════════════════════════════════════════════════════
// 이미지 뷰어 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct ImageViewerColors {
    // === 프레임 ===
    pub bg: Color,         // 배경 (빈 영역)
    pub border: Color,     // 테두리
    pub title_text: Color, // 제목 텍스트 (파일명, 해상도, 줌)

    // === 로딩 상태 ===
    pub loading_spinner: Color, // 로딩 스피너
    pub loading_text: Color,    // "Loading image..." 텍스트

    // === 에러 상태 ===
    pub error_text: Color, // 에러 메시지
    pub hint_text: Color,  // "Press ESC to close" 힌트

    // === 하단 도움말 ===
    pub footer_key: Color,       // 단축키 (PgUp, +, -, r, Esc)
    pub footer_text: Color,      // 설명 (Prev/Next, Zoom, Pan)
    pub footer_separator: Color, // 구분자 (/)
}

// ═══════════════════════════════════════════════════════════════════════════════
// 파일 정보 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct FileInfoColors {
    // === 다이얼로그 프레임 ===
    pub bg: Color,
    pub border: Color,
    pub title: Color,

    // === 정보 표시 ===
    pub label: Color,            // 라벨 (Name, Path, Type 등)
    pub value: Color,            // 기본 값
    pub value_name: Color,       // 파일/폴더 이름
    pub value_path: Color,       // 경로
    pub value_type: Color,       // 파일 타입
    pub value_size: Color,       // 크기 (숫자)
    pub value_permission: Color, // 권한
    pub value_owner: Color,      // 소유자/그룹
    pub value_date: Color,       // 날짜/시간

    // === 상태 표시 ===
    pub calculating_spinner: Color, // 계산 중 스피너
    pub calculating_text: Color,    // "Calculating..." 텍스트
    pub error_text: Color,          // 에러 메시지

    // === 하단 도움말 ===
    pub hint_text: Color, // 도움말 텍스트
}

// ═══════════════════════════════════════════════════════════════════════════════
// 도움말 화면 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct HelpColors {
    // === 다이얼로그 프레임 ===
    pub bg: Color,
    pub border: Color,
    pub title: Color,

    // === 섹션 ===
    pub section_title: Color,     // 섹션 제목 (Navigation, Tools 등)
    pub section_decorator: Color, // 섹션 데코레이터 ("──")

    // === 단축키 목록 ===
    pub key: Color,           // 단축키 텍스트
    pub key_highlight: Color, // 강조 단축키 (첫 글자)
    pub description: Color,   // 설명 텍스트

    // === 하단 도움말 ===
    pub hint_text: Color, // "Press any key to close"
}

// ═══════════════════════════════════════════════════════════════════════════════
// 고급 검색 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct AdvancedSearchColors {
    pub bg: Color,
    pub border: Color,
    pub title: Color,
    pub label: Color,
    pub input_text: Color,
    pub input_cursor: Color,
    pub field_bracket: Color, // 필드 괄호 색상
    pub checkbox_checked: Color,
    pub checkbox_unchecked: Color,
    pub button_text: Color,
    pub button_selected_bg: Color,
    pub button_selected_text: Color,
    pub footer_key: Color,
    pub footer_text: Color,
}

// ═══════════════════════════════════════════════════════════════════════════════
// DIFF 화면 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct DiffColors {
    pub bg: Color,
    pub border: Color,
    pub header_text: Color,
    pub header_label: Color,
    pub column_header_bg: Color,
    pub column_header_text: Color,
    pub same_text: Color,
    pub modified_text: Color,
    pub modified_bg: Color,
    pub left_only_text: Color,
    pub left_only_bg: Color,
    pub right_only_text: Color,
    pub right_only_bg: Color,
    pub empty_bg: Color,
    pub dir_same_text: Color,
    pub dir_modified_text: Color,
    pub cursor_bg: Color,
    pub cursor_text: Color,
    pub marked_text: Color,
    pub size_text: Color,
    pub date_text: Color,
    pub status_bar_bg: Color,
    pub status_bar_text: Color,
    pub filter_label: Color,
    pub stats_text: Color,
    pub footer_key: Color,
    pub footer_text: Color,
    pub panel_selected_border: Color,
    pub progress_spinner: Color,
    pub progress_bar_fill: Color,
    pub progress_bar_empty: Color,
    pub progress_percent_text: Color,
    pub progress_value_text: Color,
    pub progress_hint_text: Color,
}

// ═══════════════════════════════════════════════════════════════════════════════
// DIFF 파일 내용 비교 색상
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
pub struct DiffFileViewColors {
    pub bg: Color,
    pub border: Color,
    pub header_text: Color,
    pub line_number: Color,
    pub same_text: Color,
    pub modified_text: Color,
    pub modified_bg: Color,
    pub left_only_text: Color,
    pub left_only_bg: Color,
    pub right_only_text: Color,
    pub right_only_bg: Color,
    pub empty_bg: Color,
    pub inline_change_bg: Color,
    pub inline_change_text: Color,
    pub status_bar_bg: Color,
    pub status_bar_text: Color,
    pub footer_key: Color,
    pub footer_text: Color,
}

#[derive(Clone, Copy)]
pub struct GitScreenColors {
    pub bg: Color,
    pub border: Color,
    pub header_branch: Color,
    pub header_path: Color,
    pub tab_active: Color,
    pub tab_inactive: Color,
    pub tab_bar_bg: Color,
    pub file_staged: Color,
    pub file_modified: Color,
    pub file_untracked: Color,
    pub file_deleted: Color,
    pub selected_bg: Color,
    pub selected_text: Color,
    pub footer_key: Color,
    pub footer_text: Color,
    pub commit_input_border: Color,
    pub commit_input_text: Color,
    pub log_hash: Color,
    pub log_message: Color,
    pub log_author: Color,
    pub log_date: Color,
    pub branch_current: Color,
    pub branch_normal: Color,
    pub diff_add: Color,
    pub diff_remove: Color,
    pub diff_header: Color,
}

#[derive(Clone, Copy)]
pub struct DedupScreenColors {
    pub bg: Color,
    pub border: Color,
    pub title: Color,
    pub phase_text: Color,
    pub stats_text: Color,
    pub progress_bar_fill: Color,
    pub progress_bar_empty: Color,
    pub progress_text: Color,
    pub log_text: Color,
    pub log_text_alt: Color,
    pub log_deleted: Color,
    pub log_error: Color,
    pub footer_key: Color,
    pub footer_text: Color,
}

// ═══════════════════════════════════════════════════════════════════════════════
// 메인 Theme 구조체
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
#[allow(dead_code)]
pub struct Theme {
    // 기본 팔레트
    pub palette: Palette,

    // 상태 색상
    pub state: StateColors,

    // UI 컴포넌트별 색상
    pub panel: PanelColors,
    pub header: HeaderColors,
    pub status_bar: StatusBarColors,
    pub function_bar: FunctionBarColors,
    pub message: MessageColors,
    pub dialog: DialogColors,
    pub confirm_dialog: ConfirmDialogColors,
    pub settings: SettingsColors,
    pub editor: EditorColors,
    pub syntax: SyntaxColors,
    pub viewer: ViewerColors,
    pub process_manager: ProcessManagerColors,
    pub ai_screen: AIScreenColors,
    pub system_info: SystemInfoColors,
    pub search_result: SearchResultColors,
    pub image_viewer: ImageViewerColors,
    pub file_info: FileInfoColors,
    pub help: HelpColors,
    pub advanced_search: AdvancedSearchColors,
    pub diff: DiffColors,
    pub diff_file_view: DiffFileViewColors,
    pub git_screen: GitScreenColors,
    pub dedup_screen: DedupScreenColors,

    // 아이콘 문자
    pub chars: ThemeChars,
}

impl Default for Theme {
    fn default() -> Self {
        Self::dawn_of_coding()
    }
}

impl Theme {
    /// Load theme by name from ~/.cokacdir/themes/{name}.json
    /// Falls back to built-in theme if file not found
    pub fn load(name: &str) -> Self {
        // Try to load from JSON file first
        if let Some(theme) = super::theme_loader::load_theme(name) {
            return theme;
        }
        // Fall back to built-in themes
        match name {
            "light" => Self::light(),
            "dark" => Self::dark(),
            "dawn_of_coding" => Self::dawn_of_coding(),
            _ => Self::dawn_of_coding(),
        }
    }

    /// Check if terminal supports true color (24-bit RGB)
    #[allow(dead_code)]
    fn supports_true_color() -> bool {
        if let Some(support) = supports_color::on(Stream::Stdout) {
            support.has_16m
        } else {
            false
        }
    }

    /// Light theme (default)
    pub fn light() -> Self {
        Self::light_256()
    }

    fn light_256() -> Self {
        // 기본 팔레트 정의
        let palette = Palette {
            // 명도 기반
            bg: Color::Indexed(255),         // 기본 배경
            bg_alt: Color::Indexed(254),     // 대체 배경
            fg: Color::Indexed(243),         // 기본 텍스트
            fg_dim: Color::Indexed(251),     // 흐린 텍스트
            fg_strong: Color::Indexed(238),  // 강조 텍스트
            fg_inverse: Color::Indexed(231), // 반전 텍스트

            // 용도 기반
            accent: Color::Indexed(21),     // 정보성 강조
            shortcut: Color::Indexed(74),   // 단축키
            positive: Color::Indexed(34),   // 긍정/성공
            highlight: Color::Indexed(198), // 강조/경고/에러
        };

        // 상태 색상
        let state = StateColors {
            success: Color::Indexed(34),
            warning: Color::Indexed(198),
            error: Color::Indexed(198),
            info: Color::Indexed(21),
        };

        // 패널 색상
        let panel = PanelColors {
            bg: Color::Indexed(255),
            border: Color::Indexed(251),
            border_active: Color::Indexed(238),
            header_bg: Color::Indexed(254),
            header_bg_active: Color::Indexed(253),
            header_text: Color::Indexed(249),
            header_text_active: Color::Indexed(242), // 활성 패널 헤더 텍스트
            file_text: Color::Indexed(243),
            directory_text: Color::Indexed(67),
            symlink_text: Color::Indexed(37), // cyan for symlinks
            selected_bg: Color::Indexed(67),
            selected_text: Color::Indexed(231),
            marked_text: Color::Indexed(198),
            size_text: Color::Indexed(251),
            date_text: Color::Indexed(251),
            remote_indicator: Color::Indexed(67),
        };

        // 앱 헤더
        let header = HeaderColors {
            bg: Color::Indexed(255),
            text: Color::Indexed(243),
            title: Color::Indexed(238),
        };

        // 상태 표시줄
        let status_bar = StatusBarColors {
            bg: Color::Indexed(253),
            text: Color::Indexed(249),
            text_dim: Color::Indexed(251),
        };

        // 함수 바
        let function_bar = FunctionBarColors {
            bg: Color::Indexed(255),
            key: Color::Indexed(243),
            label: Color::Indexed(251),
        };

        // 메시지
        let message = MessageColors {
            bg: Color::Indexed(255),
            text: Color::Indexed(198),
        };

        // 대화 상자
        let dialog = DialogColors {
            // === 다이얼로그 프레임 ===
            bg: Color::Indexed(255),
            border: Color::Indexed(238),
            title: Color::Indexed(238),

            // === 일반 텍스트 ===
            text: Color::Indexed(243),
            text_dim: Color::Indexed(251),
            message_text: Color::Indexed(243),

            // === 입력 필드 ===
            input_text: Color::Indexed(243),
            input_cursor_fg: Color::Indexed(255),
            input_cursor_bg: Color::Indexed(238),
            input_prompt: Color::Indexed(74), // 단축키 색상 (editor.footer_key)

            // === 버튼 ===
            button_text: Color::Indexed(251),
            button_selected_bg: Color::Indexed(67),
            button_selected_text: Color::Indexed(231),

            // === 자동완성 ===
            autocomplete_bg: Color::Indexed(255),
            autocomplete_text: Color::Indexed(243),
            autocomplete_directory_text: Color::Indexed(67),
            autocomplete_selected_bg: Color::Indexed(67),
            autocomplete_selected_text: Color::Indexed(231),
            autocomplete_scroll_info: Color::Indexed(251),
            preview_suffix_text: Color::Indexed(251),

            // === 도움말 ===
            help_key_text: Color::Indexed(74), // 단축키 색상 (editor.footer_key)
            help_label_text: Color::Indexed(251),

            // === 진행률 ===
            progress_label_text: Color::Indexed(251),
            progress_value_text: Color::Indexed(243),
            progress_bar_fill: Color::Indexed(67), // 선택 배경색 (panel.selected_bg)
            progress_bar_empty: Color::Indexed(251),
            progress_percent_text: Color::Indexed(243),

            // === 충돌 ===
            conflict_filename_text: Color::Indexed(198), // 강조된 파일명
            conflict_count_text: Color::Indexed(251),    // 진행 정보
            conflict_shortcut_text: Color::Indexed(117), // 버튼 단축키 (O, S, A, l)

            // === Tar 제외 확인 ===
            tar_exclude_title: Color::Indexed(238), // 제목 (dialog.title과 동일)
            tar_exclude_border: Color::Indexed(238), // 테두리 (dialog.border와 동일)
            tar_exclude_bg: Color::Indexed(255),    // 배경 (dialog.bg와 동일)
            tar_exclude_message_text: Color::Indexed(243), // 메시지 텍스트 (dialog.message_text와 동일)
            tar_exclude_path_text: Color::Indexed(208),    // 제외 경로 (주황색)
            tar_exclude_scroll_info: Color::Indexed(251),  // 스크롤 정보
            tar_exclude_button_text: Color::Indexed(251), // 버튼 텍스트 (dialog.button_text와 동일)
            tar_exclude_button_selected_bg: Color::Indexed(67), // 선택 버튼 배경
            tar_exclude_button_selected_text: Color::Indexed(231), // 선택 버튼 텍스트

            // === Git Log Diff ===
            git_log_diff_title: Color::Indexed(238),
            git_log_diff_border: Color::Indexed(238),
            git_log_diff_bg: Color::Indexed(255),
            git_log_diff_message_text: Color::Indexed(243),
            git_log_diff_entry_text: Color::Indexed(243),
            git_log_diff_selected_text: Color::Indexed(34),
            git_log_diff_cursor_text: Color::Indexed(231),
            git_log_diff_cursor_bg: Color::Indexed(67),
            git_log_diff_button_text: Color::Indexed(251),
            git_log_diff_button_selected_text: Color::Indexed(231),
            git_log_diff_button_selected_bg: Color::Indexed(67),
            git_log_diff_button_disabled_text: Color::Indexed(251),
            git_log_diff_scroll_info: Color::Indexed(251),
            remote_bookmark_text: Color::Indexed(67),
            remote_connect_field_label: Color::Indexed(243),
            remote_connect_field_value: Color::Indexed(238),
            remote_connect_field_selected_bg: Color::Indexed(67),
        };

        // 확인 다이얼로그 (Large File/Image Confirm)
        let confirm_dialog = ConfirmDialogColors {
            bg: Color::Indexed(255),
            border: Color::Indexed(238),
            title: Color::Indexed(238),
            message_text: Color::Indexed(243),
            button_text: Color::Indexed(251),
            button_selected_bg: Color::Indexed(67),
            button_selected_text: Color::Indexed(231),
        };

        // 설정 다이얼로그
        let settings = SettingsColors {
            bg: Color::Indexed(255),
            border: Color::Indexed(238),
            title: Color::Indexed(238),
            label_text: Color::Indexed(243),
            prompt: Color::Indexed(74),
            value_text: Color::Indexed(231),
            value_bg: Color::Indexed(67),
            help_key: Color::Indexed(74),
            help_text: Color::Indexed(251),
        };

        // 에디터
        let editor = EditorColors {
            bg: Color::Indexed(255),
            border: Color::Indexed(238),
            header_bg: Color::Indexed(253),
            header_text: Color::Indexed(249),
            header_info: Color::Indexed(251),
            line_number: Color::Indexed(251),
            text: Color::Indexed(243),
            cursor: Color::Indexed(238),
            selection_bg: Color::Indexed(67),
            selection_text: Color::Indexed(231),
            match_bg: Color::Indexed(198),
            match_current_bg: Color::Indexed(208),
            bracket_match: Color::Indexed(74),
            modified_mark: Color::Indexed(198),
            footer_bg: Color::Indexed(253),
            footer_key: Color::Indexed(74),
            footer_text: Color::Indexed(251),
            find_input_text: Color::Indexed(243),
            find_option: Color::Indexed(251),
            find_option_active: Color::Indexed(74),
            wrap_indicator: Color::Indexed(248),
            remote_path_text: Color::Indexed(214),
        };

        // 코드 하이라이팅 (라이트 테마)
        let syntax = SyntaxColors {
            keyword: Color::Indexed(127),   // 보라색 (if, else, fn, let)
            type_name: Color::Indexed(37),  // 청록색 (String, i32, Vec)
            string: Color::Indexed(28),     // 녹색 ("hello")
            number: Color::Indexed(166),    // 주황색 (123, 3.14)
            comment: Color::Indexed(102),   // 회색 (// comment)
            operator: Color::Indexed(241),  // 진한 회색 (+, -, =)
            function: Color::Indexed(130),  // 갈색/주황 (function names)
            macro_name: Color::Indexed(91), // 자주색 (println!, vec!)
            attribute: Color::Indexed(243), // 회색 (#[derive])
            variable: Color::Indexed(236),  // 진한 회색 (variables)
            constant: Color::Indexed(161),  // 마젠타 (CONST, true, false)
            bracket: Color::Indexed(240),   // 회색 ((), [], {})
            normal: Color::Indexed(236),    // 기본 텍스트
        };

        // 뷰어
        let viewer = ViewerColors {
            bg: Color::Indexed(255),
            border: Color::Indexed(238),
            header_text: Color::Indexed(249),
            line_number: Color::Indexed(251),
            text: Color::Indexed(243),
            bookmark_indicator: Color::Indexed(21), // 북마크 표시 색상
            search_input_text: Color::Indexed(67),
            search_cursor_fg: Color::Indexed(255),
            search_cursor_bg: Color::Indexed(67),
            search_match_current_bg: Color::Indexed(67),
            search_match_current_fg: Color::Indexed(255),
            search_match_other_bg: Color::Indexed(243),
            search_match_other_fg: Color::Indexed(255),
            search_info: Color::Indexed(251),
            hex_offset: Color::Indexed(251),
            hex_bytes: Color::Indexed(243),
            hex_ascii: Color::Indexed(238),
            wrap_indicator: Color::Indexed(248),
            footer_key: Color::Indexed(74),
            footer_text: Color::Indexed(251),
        };

        // 프로세스 관리자
        let process_manager = ProcessManagerColors {
            bg: Color::Indexed(255),
            border: Color::Indexed(238),
            header_text: Color::Indexed(249),
            column_header: Color::Indexed(21),
            text: Color::Indexed(243),
            selected_bg: Color::Indexed(67),
            selected_text: Color::Indexed(231),
            cpu_high: Color::Indexed(198),
            mem_high: Color::Indexed(198),
            confirm_text: Color::Indexed(198),
            footer_key: Color::Indexed(74),
            footer_text: Color::Indexed(251),
        };

        // AI 화면 (Panel/Viewer/Editor 색상만 사용)
        let ai_screen = AIScreenColors {
            // === 배경 ===
            bg: Color::Indexed(255), // 흰색 배경 (editor.bg)

            // === 히스토리 영역 ===
            history_border: Color::Indexed(238), // 테두리 (editor.border)
            history_title: Color::Indexed(238),  // 제목 (editor.border)
            history_placeholder: Color::Indexed(251), // 플레이스홀더 (editor.footer_text)
            history_scroll_info: Color::Indexed(251), // 스크롤 정보 (editor.footer_text)

            // === 메시지 프리픽스 ===
            user_prefix: Color::Indexed(67), // 사용자 ">" (panel.directory_text)
            assistant_prefix: Color::Indexed(74), // AI "<" (editor.footer_key)
            error_prefix: Color::Indexed(198), // 에러 "!" (panel.marked_text)
            system_prefix: Color::Indexed(251), // 시스템 "*" (editor.footer_text)

            // === 메시지 내용 ===
            message_text: Color::Indexed(243), // 메시지 텍스트 (editor.text)

            // === 입력 영역 ===
            input_border: Color::Indexed(238), // 입력 테두리 (editor.border)
            input_prompt: Color::Indexed(74),  // 입력 ">" (editor.footer_key)
            input_text: Color::Indexed(243),   // 입력 텍스트 (editor.text)
            input_cursor_fg: Color::Indexed(255), // 커서 위 문자 (흰색 - 반전)
            input_cursor_bg: Color::Indexed(238), // 커서 배경 (어두운색)
            input_placeholder: Color::Indexed(251), // 플레이스홀더 (editor.footer_text)

            // === 처리 중 상태 ===
            processing_spinner: Color::Indexed(74), // 스피너 (editor.footer_key)
            processing_text: Color::Indexed(251),   // 처리 중 텍스트 (editor.footer_text)

            // === 에러 상태 ===
            error_text: Color::Indexed(198), // 에러 텍스트 (panel.marked_text)

            // === 도구 사용 표시 ===
            tool_use_prefix: Color::Indexed(136), // "[]" 도구 브래킷 (황색)
            tool_use_name: Color::Indexed(67),    // 도구 이름 (디렉토리 색상)
            tool_use_input: Color::Indexed(243),  // 도구 입력 (일반 텍스트)
            tool_result_prefix: Color::Indexed(34), // "->" 결과 프리픽스 (녹색)
            tool_result_text: Color::Indexed(243), // 결과 텍스트 (일반 텍스트)

            // === 하단 도움말 ===
            footer_key: Color::Indexed(74), // 단축키 (editor.footer_key)
            footer_text: Color::Indexed(251), // 설명 (editor.footer_text)
        };

        // 시스템 정보
        let system_info = SystemInfoColors {
            bg: Color::Indexed(255),
            border: Color::Indexed(238),
            section_title: Color::Indexed(34),
            label: Color::Indexed(243),
            value: Color::Indexed(243),
            bar_fill: Color::Indexed(34),
            bar_empty: Color::Indexed(251),
            usage_low: Color::Indexed(34),     // 성공 색상 (녹색)
            usage_medium: Color::Indexed(198), // 경고 색상 (핑크)
            usage_high: Color::Indexed(198),   // 에러 색상 (핑크)
            tab_active: Color::Indexed(238),   // 활성 테두리 색상
            disk_header: Color::Indexed(21),
            disk_text: Color::Indexed(243),
            selected_bg: Color::Indexed(67),
            selected_text: Color::Indexed(231),
            footer_key: Color::Indexed(74),
            footer_text: Color::Indexed(251),
        };

        // 검색 결과
        let search_result = SearchResultColors {
            bg: Color::Indexed(255),
            border: Color::Indexed(238),
            header_text: Color::Indexed(249),
            column_header: Color::Indexed(21),
            column_header_dim: Color::Indexed(251), // 흐린 컬럼 헤더
            directory_text: Color::Indexed(238),
            file_text: Color::Indexed(243),
            selected_bg: Color::Indexed(67),
            selected_text: Color::Indexed(231),
            match_highlight: Color::Indexed(198),
            path_text: Color::Indexed(251),
            footer_key: Color::Indexed(74),
            footer_text: Color::Indexed(251),
        };

        // 이미지 뷰어
        let image_viewer = ImageViewerColors {
            // === 프레임 ===
            bg: Color::Indexed(255),         // 배경 (viewer.bg)
            border: Color::Indexed(238),     // 테두리 (viewer.border)
            title_text: Color::Indexed(249), // 제목 텍스트 (viewer.header_text)

            // === 로딩 상태 ===
            loading_spinner: Color::Indexed(74), // 스피너 (shortcut 색상)
            loading_text: Color::Indexed(251),   // 로딩 텍스트

            // === 에러 상태 ===
            error_text: Color::Indexed(198), // 에러 (highlight 색상)
            hint_text: Color::Indexed(251),  // 힌트 텍스트

            // === 하단 도움말 ===
            footer_key: Color::Indexed(74),   // 단축키 (shortcut 색상)
            footer_text: Color::Indexed(251), // 설명
            footer_separator: Color::Indexed(251), // 구분자
        };

        // 파일 정보
        let file_info = FileInfoColors {
            // === 다이얼로그 프레임 ===
            bg: Color::Indexed(255),
            border: Color::Indexed(238),
            title: Color::Indexed(238),

            // === 정보 표시 ===
            label: Color::Indexed(251),
            value: Color::Indexed(243),
            value_name: Color::Indexed(67), // 파일명은 폴더색 (파란)
            value_path: Color::Indexed(243),
            value_type: Color::Indexed(243),
            value_size: Color::Indexed(67), // 크기는 숫자 강조 (파란)
            value_permission: Color::Indexed(243),
            value_owner: Color::Indexed(243),
            value_date: Color::Indexed(243),

            // === 상태 표시 ===
            calculating_spinner: Color::Indexed(74),
            calculating_text: Color::Indexed(74),
            error_text: Color::Indexed(198),

            // === 하단 도움말 ===
            hint_text: Color::Indexed(251),
        };

        // 도움말
        let help = HelpColors {
            // === 다이얼로그 프레임 ===
            bg: Color::Indexed(255),
            border: Color::Indexed(238),
            title: Color::Indexed(238),

            // === 섹션 ===
            section_title: Color::Indexed(67), // 섹션 제목 (파란)
            section_decorator: Color::Indexed(251), // 섹션 데코레이터 ("──")

            // === 단축키 목록 ===
            key: Color::Indexed(74),           // 단축키 텍스트 (청록)
            key_highlight: Color::Indexed(74), // 강조 단축키 (청록)
            description: Color::Indexed(243),  // 설명 텍스트

            // === 하단 도움말 ===
            hint_text: Color::Indexed(251),
        };

        // 고급 검색
        let advanced_search = AdvancedSearchColors {
            bg: Color::Indexed(255),
            border: Color::Indexed(238),
            title: Color::Indexed(238),
            label: Color::Indexed(243),
            input_text: Color::Indexed(243),
            input_cursor: Color::Indexed(238),
            field_bracket: Color::Indexed(21), // 필드 괄호 색상
            checkbox_checked: Color::Indexed(34),
            checkbox_unchecked: Color::Indexed(251),
            button_text: Color::Indexed(251),
            button_selected_bg: Color::Indexed(67),
            button_selected_text: Color::Indexed(231),
            footer_key: Color::Indexed(74),
            footer_text: Color::Indexed(251),
        };

        let diff = DiffColors {
            bg: Color::Indexed(255),
            border: Color::Indexed(249),
            header_text: Color::Indexed(238),
            header_label: Color::Indexed(67),
            column_header_bg: Color::Indexed(254),
            column_header_text: Color::Indexed(238),
            same_text: Color::Indexed(243),
            modified_text: Color::Indexed(167),
            modified_bg: Color::Indexed(224),
            left_only_text: Color::Indexed(25),
            left_only_bg: Color::Indexed(153),
            right_only_text: Color::Indexed(25),
            right_only_bg: Color::Indexed(153),
            empty_bg: Color::Indexed(254),
            dir_same_text: Color::Indexed(67),
            dir_modified_text: Color::Indexed(167),
            cursor_bg: Color::Indexed(67),
            cursor_text: Color::Indexed(231),
            marked_text: Color::Indexed(198),
            size_text: Color::Indexed(251),
            date_text: Color::Indexed(251),
            status_bar_bg: Color::Indexed(253),
            status_bar_text: Color::Indexed(243),
            filter_label: Color::Indexed(67),
            stats_text: Color::Indexed(243),
            footer_key: Color::Indexed(74),
            footer_text: Color::Indexed(251),
            panel_selected_border: Color::Indexed(198),
            progress_spinner: Color::Indexed(67),
            progress_bar_fill: Color::Indexed(67),
            progress_bar_empty: Color::Indexed(251),
            progress_percent_text: Color::Indexed(243),
            progress_value_text: Color::Indexed(243),
            progress_hint_text: Color::Indexed(251),
        };

        let diff_file_view = DiffFileViewColors {
            bg: Color::Indexed(255),
            border: Color::Indexed(238),
            header_text: Color::Indexed(238),
            line_number: Color::Indexed(251),
            same_text: Color::Indexed(243),
            modified_text: Color::Indexed(124),
            modified_bg: Color::Indexed(224),
            left_only_text: Color::Indexed(19),
            left_only_bg: Color::Indexed(153),
            right_only_text: Color::Indexed(19),
            right_only_bg: Color::Indexed(153),
            empty_bg: Color::Indexed(254),
            inline_change_bg: Color::Indexed(217),
            inline_change_text: Color::Indexed(124),
            status_bar_bg: Color::Indexed(253),
            status_bar_text: Color::Indexed(243),
            footer_key: Color::Indexed(74),
            footer_text: Color::Indexed(251),
        };

        let git_screen = GitScreenColors {
            bg: Color::Indexed(255),
            border: Color::Indexed(238),
            header_branch: Color::Indexed(34),
            header_path: Color::Indexed(243),
            tab_active: Color::Indexed(21),
            tab_inactive: Color::Indexed(251),
            tab_bar_bg: Color::Indexed(254),
            file_staged: Color::Indexed(34),
            file_modified: Color::Indexed(136),
            file_untracked: Color::Indexed(198),
            file_deleted: Color::Indexed(124),
            selected_bg: Color::Indexed(67),
            selected_text: Color::Indexed(231),
            footer_key: Color::Indexed(74),
            footer_text: Color::Indexed(251),
            commit_input_border: Color::Indexed(238),
            commit_input_text: Color::Indexed(243),
            log_hash: Color::Indexed(67),
            log_message: Color::Indexed(243),
            log_author: Color::Indexed(21),
            log_date: Color::Indexed(251),
            branch_current: Color::Indexed(34),
            branch_normal: Color::Indexed(243),
            diff_add: Color::Indexed(34),
            diff_remove: Color::Indexed(198),
            diff_header: Color::Indexed(21),
        };

        let dedup_screen = DedupScreenColors {
            bg: Color::Indexed(255),
            border: Color::Indexed(238),
            title: Color::Indexed(21),
            phase_text: Color::Indexed(34),
            stats_text: Color::Indexed(243),
            progress_bar_fill: Color::Indexed(34),
            progress_bar_empty: Color::Indexed(254),
            progress_text: Color::Indexed(243),
            log_text: Color::Indexed(243),
            log_text_alt: Color::Indexed(249),
            log_deleted: Color::Indexed(198),
            log_error: Color::Indexed(124),
            footer_key: Color::Indexed(74),
            footer_text: Color::Indexed(251),
        };

        Self {
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

    /// Dark theme
    pub fn dark() -> Self {
        Self::dark_256()
    }

    fn dark_256() -> Self {
        // 기본 팔레트 정의 (어두운 배경)
        let palette = Palette {
            bg: Color::Indexed(235),         // 어두운 배경
            bg_alt: Color::Indexed(236),     // 대체 배경
            fg: Color::Indexed(252),         // 밝은 텍스트
            fg_dim: Color::Indexed(245),     // 흐린 텍스트
            fg_strong: Color::Indexed(255),  // 강조 텍스트
            fg_inverse: Color::Indexed(235), // 반전 텍스트
            accent: Color::Indexed(81),      // 정보성 강조
            shortcut: Color::Indexed(117),   // 단축키
            positive: Color::Indexed(114),   // 긍정/성공
            highlight: Color::Indexed(204),  // 강조/경고/에러
        };

        let state = StateColors {
            success: Color::Indexed(114),
            warning: Color::Indexed(204),
            error: Color::Indexed(204),
            info: Color::Indexed(81),
        };

        let panel = PanelColors {
            bg: Color::Indexed(235),
            border: Color::Indexed(245),
            border_active: Color::Indexed(252),
            header_bg: Color::Indexed(236),
            header_bg_active: Color::Indexed(237),
            header_text: Color::Indexed(250),
            header_text_active: Color::Indexed(255), // 활성 패널 헤더 텍스트
            file_text: Color::Indexed(252),
            directory_text: Color::Indexed(117),
            symlink_text: Color::Indexed(44), // cyan for symlinks
            selected_bg: Color::Indexed(117),
            selected_text: Color::Indexed(16),
            marked_text: Color::Indexed(204),
            size_text: Color::Indexed(245),
            date_text: Color::Indexed(245),
            remote_indicator: Color::Indexed(117),
        };

        let header = HeaderColors {
            bg: Color::Indexed(235),
            text: Color::Indexed(252),
            title: Color::Indexed(255),
        };

        let status_bar = StatusBarColors {
            bg: Color::Indexed(237),
            text: Color::Indexed(250),
            text_dim: Color::Indexed(245),
        };

        let function_bar = FunctionBarColors {
            bg: Color::Indexed(235),
            key: Color::Indexed(252),
            label: Color::Indexed(245),
        };

        let message = MessageColors {
            bg: Color::Indexed(235),
            text: Color::Indexed(204),
        };

        let dialog = DialogColors {
            bg: Color::Indexed(236),
            border: Color::Indexed(252),
            title: Color::Indexed(255),
            text: Color::Indexed(252),
            text_dim: Color::Indexed(245),
            message_text: Color::Indexed(252),
            input_text: Color::Indexed(252),
            input_cursor_fg: Color::Indexed(235),
            input_cursor_bg: Color::Indexed(252),
            input_prompt: Color::Indexed(117),
            button_text: Color::Indexed(245),
            button_selected_bg: Color::Indexed(117),
            button_selected_text: Color::Indexed(235),
            autocomplete_bg: Color::Indexed(236),
            autocomplete_text: Color::Indexed(252),
            autocomplete_directory_text: Color::Indexed(117),
            autocomplete_selected_bg: Color::Indexed(117),
            autocomplete_selected_text: Color::Indexed(235),
            autocomplete_scroll_info: Color::Indexed(245),
            preview_suffix_text: Color::Indexed(245),
            help_key_text: Color::Indexed(117),
            help_label_text: Color::Indexed(245),
            progress_label_text: Color::Indexed(245),
            progress_value_text: Color::Indexed(252),
            progress_bar_fill: Color::Indexed(117),
            progress_bar_empty: Color::Indexed(245),
            progress_percent_text: Color::Indexed(252),
            conflict_filename_text: Color::Indexed(204), // 강조된 파일명
            conflict_count_text: Color::Indexed(245),    // 진행 정보
            conflict_shortcut_text: Color::Indexed(33),  // 버튼 단축키 (O, S, A, l)

            // === Tar 제외 확인 ===
            tar_exclude_title: Color::Indexed(255), // 제목 (dialog.title과 동일)
            tar_exclude_border: Color::Indexed(252), // 테두리 (dialog.border와 동일)
            tar_exclude_bg: Color::Indexed(236),    // 배경 (dialog.bg와 동일)
            tar_exclude_message_text: Color::Indexed(252), // 메시지 텍스트 (dialog.message_text와 동일)
            tar_exclude_path_text: Color::Indexed(166),    // 제외 경로 (주황색)
            tar_exclude_scroll_info: Color::Indexed(245),  // 스크롤 정보
            tar_exclude_button_text: Color::Indexed(245), // 버튼 텍스트 (dialog.button_text와 동일)
            tar_exclude_button_selected_bg: Color::Indexed(117), // 선택 버튼 배경
            tar_exclude_button_selected_text: Color::Indexed(235), // 선택 버튼 텍스트

            // === Git Log Diff ===
            git_log_diff_title: Color::Indexed(255),
            git_log_diff_border: Color::Indexed(252),
            git_log_diff_bg: Color::Indexed(236),
            git_log_diff_message_text: Color::Indexed(252),
            git_log_diff_entry_text: Color::Indexed(252),
            git_log_diff_selected_text: Color::Indexed(84),
            git_log_diff_cursor_text: Color::Indexed(235),
            git_log_diff_cursor_bg: Color::Indexed(117),
            git_log_diff_button_text: Color::Indexed(245),
            git_log_diff_button_selected_text: Color::Indexed(235),
            git_log_diff_button_selected_bg: Color::Indexed(117),
            git_log_diff_button_disabled_text: Color::Indexed(242),
            git_log_diff_scroll_info: Color::Indexed(245),
            remote_bookmark_text: Color::Indexed(117),
            remote_connect_field_label: Color::Indexed(252),
            remote_connect_field_value: Color::Indexed(255),
            remote_connect_field_selected_bg: Color::Indexed(117),
        };

        // 확인 다이얼로그 (Large File/Image Confirm)
        let confirm_dialog = ConfirmDialogColors {
            bg: Color::Indexed(238),
            border: Color::Indexed(253),
            title: Color::Indexed(255),
            message_text: Color::Indexed(253),
            button_text: Color::Indexed(245),
            button_selected_bg: Color::Indexed(61),
            button_selected_text: Color::Indexed(255),
        };

        let settings = SettingsColors {
            bg: Color::Indexed(236),
            border: Color::Indexed(252),
            title: Color::Indexed(255),
            label_text: Color::Indexed(252),
            prompt: Color::Indexed(117),
            value_text: Color::Indexed(235),
            value_bg: Color::Indexed(117),
            help_key: Color::Indexed(117),
            help_text: Color::Indexed(245),
        };

        let editor = EditorColors {
            bg: Color::Indexed(235),
            border: Color::Indexed(252),
            header_bg: Color::Indexed(237),
            header_text: Color::Indexed(250),
            header_info: Color::Indexed(245),
            line_number: Color::Indexed(245),
            text: Color::Indexed(252),
            cursor: Color::Indexed(252),
            selection_bg: Color::Indexed(117),
            selection_text: Color::Indexed(16),
            match_bg: Color::Indexed(204),
            match_current_bg: Color::Indexed(208),
            bracket_match: Color::Indexed(117),
            modified_mark: Color::Indexed(204),
            footer_bg: Color::Indexed(237),
            footer_key: Color::Indexed(117),
            footer_text: Color::Indexed(245),
            find_input_text: Color::Indexed(117),
            find_option: Color::Indexed(245),
            find_option_active: Color::Indexed(117),
            wrap_indicator: Color::Indexed(240),
            remote_path_text: Color::Indexed(214),
        };

        // 코드 하이라이팅 (다크 테마)
        let syntax = SyntaxColors {
            keyword: Color::Indexed(176),    // 연한 보라색 (if, else, fn, let)
            type_name: Color::Indexed(81),   // 밝은 청록색 (String, i32, Vec)
            string: Color::Indexed(114),     // 밝은 녹색 ("hello")
            number: Color::Indexed(209),     // 밝은 주황색 (123, 3.14)
            comment: Color::Indexed(102),    // 회색 (// comment)
            operator: Color::Indexed(252),   // 밝은 회색 (+, -, =)
            function: Color::Indexed(222),   // 노란색 (function names)
            macro_name: Color::Indexed(141), // 밝은 자주색 (println!, vec!)
            attribute: Color::Indexed(245),  // 회색 (#[derive])
            variable: Color::Indexed(252),   // 밝은 회색 (variables)
            constant: Color::Indexed(210),   // 밝은 빨강/핑크 (CONST, true, false)
            bracket: Color::Indexed(250),    // 밝은 회색 ((), [], {})
            normal: Color::Indexed(252),     // 기본 텍스트
        };

        let viewer = ViewerColors {
            bg: Color::Indexed(235),
            border: Color::Indexed(252),
            header_text: Color::Indexed(250),
            line_number: Color::Indexed(245),
            text: Color::Indexed(252),
            bookmark_indicator: Color::Indexed(81), // 북마크 표시 색상
            search_input_text: Color::Indexed(117),
            search_cursor_fg: Color::Indexed(235),
            search_cursor_bg: Color::Indexed(117),
            search_match_current_bg: Color::Indexed(117),
            search_match_current_fg: Color::Indexed(235),
            search_match_other_bg: Color::Indexed(245),
            search_match_other_fg: Color::Indexed(235),
            search_info: Color::Indexed(245),
            hex_offset: Color::Indexed(245),
            hex_bytes: Color::Indexed(252),
            hex_ascii: Color::Indexed(255),
            wrap_indicator: Color::Indexed(240),
            footer_key: Color::Indexed(117),
            footer_text: Color::Indexed(245),
        };

        let process_manager = ProcessManagerColors {
            bg: Color::Indexed(235),
            border: Color::Indexed(252),
            header_text: Color::Indexed(250),
            column_header: Color::Indexed(81),
            text: Color::Indexed(252),
            selected_bg: Color::Indexed(117),
            selected_text: Color::Indexed(16),
            cpu_high: Color::Indexed(204),
            mem_high: Color::Indexed(204),
            confirm_text: Color::Indexed(204),
            footer_key: Color::Indexed(117),
            footer_text: Color::Indexed(245),
        };

        let ai_screen = AIScreenColors {
            bg: Color::Indexed(235),
            history_border: Color::Indexed(252),
            history_title: Color::Indexed(255),
            history_placeholder: Color::Indexed(245),
            history_scroll_info: Color::Indexed(245),
            user_prefix: Color::Indexed(117),
            assistant_prefix: Color::Indexed(117),
            error_prefix: Color::Indexed(204),
            system_prefix: Color::Indexed(245),
            message_text: Color::Indexed(252),
            input_border: Color::Indexed(252),
            input_prompt: Color::Indexed(117),
            input_text: Color::Indexed(252),
            input_cursor_fg: Color::Indexed(235), // 커서 위 문자 (배경색으로 반전)
            input_cursor_bg: Color::Indexed(252), // 커서 배경 (밝은색)
            input_placeholder: Color::Indexed(245),
            processing_spinner: Color::Indexed(117),
            processing_text: Color::Indexed(245),
            error_text: Color::Indexed(204),
            tool_use_prefix: Color::Indexed(179), // "[]" 도구 브래킷 (황색)
            tool_use_name: Color::Indexed(81),    // 도구 이름 (시안)
            tool_use_input: Color::Indexed(252),  // 도구 입력 (일반 텍스트)
            tool_result_prefix: Color::Indexed(114), // "->" 결과 프리픽스 (녹색)
            tool_result_text: Color::Indexed(252), // 결과 텍스트 (일반 텍스트)
            footer_key: Color::Indexed(117),
            footer_text: Color::Indexed(245),
        };

        let system_info = SystemInfoColors {
            bg: Color::Indexed(235),
            border: Color::Indexed(252),
            section_title: Color::Indexed(114),
            label: Color::Indexed(252),
            value: Color::Indexed(252),
            bar_fill: Color::Indexed(114),
            bar_empty: Color::Indexed(245),
            usage_low: Color::Indexed(114),    // 성공 색상 (녹색)
            usage_medium: Color::Indexed(204), // 경고 색상 (핑크)
            usage_high: Color::Indexed(204),   // 에러 색상 (핑크)
            tab_active: Color::Indexed(252),   // 활성 테두리 색상
            disk_header: Color::Indexed(81),
            disk_text: Color::Indexed(252),
            selected_bg: Color::Indexed(117),
            selected_text: Color::Indexed(16),
            footer_key: Color::Indexed(117),
            footer_text: Color::Indexed(245),
        };

        let search_result = SearchResultColors {
            bg: Color::Indexed(235),
            border: Color::Indexed(252),
            header_text: Color::Indexed(250),
            column_header: Color::Indexed(81),
            column_header_dim: Color::Indexed(245), // 흐린 컬럼 헤더
            directory_text: Color::Indexed(255),
            file_text: Color::Indexed(252),
            selected_bg: Color::Indexed(117),
            selected_text: Color::Indexed(16),
            match_highlight: Color::Indexed(204),
            path_text: Color::Indexed(245),
            footer_key: Color::Indexed(117),
            footer_text: Color::Indexed(245),
        };

        let image_viewer = ImageViewerColors {
            bg: Color::Indexed(235),         // 배경 (viewer.bg)
            border: Color::Indexed(252),     // 테두리 (viewer.border)
            title_text: Color::Indexed(250), // 제목 텍스트 (viewer.header_text)
            loading_spinner: Color::Indexed(117),
            loading_text: Color::Indexed(245),
            error_text: Color::Indexed(204),
            hint_text: Color::Indexed(245),
            footer_key: Color::Indexed(117),
            footer_text: Color::Indexed(245),
            footer_separator: Color::Indexed(245),
        };

        let file_info = FileInfoColors {
            bg: Color::Indexed(235),
            border: Color::Indexed(252),
            title: Color::Indexed(255),
            label: Color::Indexed(245),
            value: Color::Indexed(252),
            value_name: Color::Indexed(117),
            value_path: Color::Indexed(252),
            value_type: Color::Indexed(252),
            value_size: Color::Indexed(117),
            value_permission: Color::Indexed(252),
            value_owner: Color::Indexed(252),
            value_date: Color::Indexed(252),
            calculating_spinner: Color::Indexed(117),
            calculating_text: Color::Indexed(117),
            error_text: Color::Indexed(204),
            hint_text: Color::Indexed(245),
        };

        let help = HelpColors {
            bg: Color::Indexed(235),
            border: Color::Indexed(252),
            title: Color::Indexed(255),
            section_title: Color::Indexed(117),
            section_decorator: Color::Indexed(245),
            key: Color::Indexed(117),
            key_highlight: Color::Indexed(117),
            description: Color::Indexed(252),
            hint_text: Color::Indexed(245),
        };

        let advanced_search = AdvancedSearchColors {
            bg: Color::Indexed(235),
            border: Color::Indexed(252),
            title: Color::Indexed(255),
            label: Color::Indexed(252),
            input_text: Color::Indexed(252),
            input_cursor: Color::Indexed(252),
            field_bracket: Color::Indexed(81), // 필드 괄호 색상
            checkbox_checked: Color::Indexed(114),
            checkbox_unchecked: Color::Indexed(245),
            button_text: Color::Indexed(245),
            button_selected_bg: Color::Indexed(117),
            button_selected_text: Color::Indexed(235),
            footer_key: Color::Indexed(117),
            footer_text: Color::Indexed(245),
        };

        let diff = DiffColors {
            bg: Color::Indexed(235),
            border: Color::Indexed(245),
            header_text: Color::Indexed(252),
            header_label: Color::Indexed(117),
            column_header_bg: Color::Indexed(236),
            column_header_text: Color::Indexed(252),
            same_text: Color::Indexed(252),
            modified_text: Color::Indexed(209),
            modified_bg: Color::Indexed(52),
            left_only_text: Color::Indexed(81),
            left_only_bg: Color::Indexed(23),
            right_only_text: Color::Indexed(81),
            right_only_bg: Color::Indexed(23),
            empty_bg: Color::Indexed(236),
            dir_same_text: Color::Indexed(117),
            dir_modified_text: Color::Indexed(209),
            cursor_bg: Color::Indexed(240),
            cursor_text: Color::Indexed(255),
            marked_text: Color::Indexed(204),
            size_text: Color::Indexed(245),
            date_text: Color::Indexed(245),
            status_bar_bg: Color::Indexed(237),
            status_bar_text: Color::Indexed(252),
            filter_label: Color::Indexed(117),
            stats_text: Color::Indexed(252),
            footer_key: Color::Indexed(117),
            footer_text: Color::Indexed(245),
            panel_selected_border: Color::Indexed(204),
            progress_spinner: Color::Indexed(117),
            progress_bar_fill: Color::Indexed(117),
            progress_bar_empty: Color::Indexed(245),
            progress_percent_text: Color::Indexed(252),
            progress_value_text: Color::Indexed(252),
            progress_hint_text: Color::Indexed(245),
        };

        let diff_file_view = DiffFileViewColors {
            bg: Color::Indexed(235),
            border: Color::Indexed(252),
            header_text: Color::Indexed(252),
            line_number: Color::Indexed(245),
            same_text: Color::Indexed(252),
            modified_text: Color::Indexed(209),
            modified_bg: Color::Indexed(52),
            left_only_text: Color::Indexed(81),
            left_only_bg: Color::Indexed(23),
            right_only_text: Color::Indexed(81),
            right_only_bg: Color::Indexed(23),
            empty_bg: Color::Indexed(236),
            inline_change_bg: Color::Indexed(88),
            inline_change_text: Color::Indexed(209),
            status_bar_bg: Color::Indexed(237),
            status_bar_text: Color::Indexed(252),
            footer_key: Color::Indexed(117),
            footer_text: Color::Indexed(245),
        };

        let git_screen = GitScreenColors {
            bg: Color::Indexed(235),
            border: Color::Indexed(245),
            header_branch: Color::Indexed(114),
            header_path: Color::Indexed(252),
            tab_active: Color::Indexed(81),
            tab_inactive: Color::Indexed(245),
            tab_bar_bg: Color::Indexed(236),
            file_staged: Color::Indexed(114),
            file_modified: Color::Indexed(220),
            file_untracked: Color::Indexed(204),
            file_deleted: Color::Indexed(209),
            selected_bg: Color::Indexed(240),
            selected_text: Color::Indexed(255),
            footer_key: Color::Indexed(117),
            footer_text: Color::Indexed(245),
            commit_input_border: Color::Indexed(245),
            commit_input_text: Color::Indexed(252),
            log_hash: Color::Indexed(117),
            log_message: Color::Indexed(252),
            log_author: Color::Indexed(81),
            log_date: Color::Indexed(245),
            branch_current: Color::Indexed(114),
            branch_normal: Color::Indexed(252),
            diff_add: Color::Indexed(114),
            diff_remove: Color::Indexed(204),
            diff_header: Color::Indexed(81),
        };

        let dedup_screen = DedupScreenColors {
            bg: Color::Indexed(235),
            border: Color::Indexed(245),
            title: Color::Indexed(81),
            phase_text: Color::Indexed(114),
            stats_text: Color::Indexed(252),
            progress_bar_fill: Color::Indexed(114),
            progress_bar_empty: Color::Indexed(237),
            progress_text: Color::Indexed(252),
            log_text: Color::Indexed(252),
            log_text_alt: Color::Indexed(246),
            log_deleted: Color::Indexed(204),
            log_error: Color::Indexed(209),
            footer_key: Color::Indexed(117),
            footer_text: Color::Indexed(245),
        };

        Self {
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

    /// Dawn of Coding theme - 어두운 파스텔톤의 새벽 코딩 테마
    pub fn dawn_of_coding() -> Self {
        // 기본 팔레트 정의 (어두운 파스텔톤)
        let palette = Palette {
            bg: Color::Indexed(234),
            bg_alt: Color::Indexed(235),
            fg: Color::Indexed(188),
            fg_dim: Color::Indexed(102),
            fg_strong: Color::Indexed(195),
            fg_inverse: Color::Indexed(234),
            accent: Color::Indexed(110),
            shortcut: Color::Indexed(146),
            positive: Color::Indexed(108),
            highlight: Color::Indexed(174),
        };

        let state = StateColors {
            success: Color::Indexed(108),
            warning: Color::Indexed(180),
            error: Color::Indexed(167),
            info: Color::Indexed(110),
        };

        let panel = PanelColors {
            bg: Color::Indexed(234),
            border: Color::Indexed(102),
            border_active: Color::Indexed(146),
            header_bg: Color::Indexed(235),
            header_bg_active: Color::Indexed(236),
            header_text: Color::Indexed(145),
            header_text_active: Color::Indexed(195),
            file_text: Color::Indexed(188),
            directory_text: Color::Indexed(110),
            symlink_text: Color::Indexed(73),
            selected_bg: Color::Indexed(146),
            selected_text: Color::Indexed(234),
            marked_text: Color::Indexed(174),
            size_text: Color::Indexed(102),
            date_text: Color::Indexed(102),
            remote_indicator: Color::Indexed(108),
        };

        let header = HeaderColors {
            bg: Color::Indexed(234),
            text: Color::Indexed(188),
            title: Color::Indexed(146),
        };

        let status_bar = StatusBarColors {
            bg: Color::Indexed(235),
            text: Color::Indexed(188),
            text_dim: Color::Indexed(102),
        };

        let function_bar = FunctionBarColors {
            bg: Color::Indexed(234),
            key: Color::Indexed(146),
            label: Color::Indexed(102),
        };

        let message = MessageColors {
            bg: Color::Indexed(235),
            text: Color::Indexed(174),
        };

        let dialog = DialogColors {
            bg: Color::Indexed(235),
            border: Color::Indexed(146),
            title: Color::Indexed(195),
            text: Color::Indexed(188),
            text_dim: Color::Indexed(102),
            message_text: Color::Indexed(188),
            input_text: Color::Indexed(188),
            input_cursor_fg: Color::Indexed(234),
            input_cursor_bg: Color::Indexed(146),
            input_prompt: Color::Indexed(110),
            button_text: Color::Indexed(102),
            button_selected_bg: Color::Indexed(60),
            button_selected_text: Color::Indexed(195),
            autocomplete_bg: Color::Indexed(235),
            autocomplete_text: Color::Indexed(188),
            autocomplete_directory_text: Color::Indexed(110),
            autocomplete_selected_bg: Color::Indexed(60),
            autocomplete_selected_text: Color::Indexed(195),
            autocomplete_scroll_info: Color::Indexed(102),
            preview_suffix_text: Color::Indexed(102),
            help_key_text: Color::Indexed(146),
            help_label_text: Color::Indexed(102),
            progress_label_text: Color::Indexed(102),
            progress_value_text: Color::Indexed(188),
            progress_bar_fill: Color::Indexed(108),
            progress_bar_empty: Color::Indexed(239),
            progress_percent_text: Color::Indexed(188),
            conflict_filename_text: Color::Indexed(174),
            conflict_count_text: Color::Indexed(102),
            conflict_shortcut_text: Color::Indexed(110),
            tar_exclude_title: Color::Indexed(195),
            tar_exclude_border: Color::Indexed(146),
            tar_exclude_bg: Color::Indexed(235),
            tar_exclude_message_text: Color::Indexed(188),
            tar_exclude_path_text: Color::Indexed(180),
            tar_exclude_scroll_info: Color::Indexed(102),
            tar_exclude_button_text: Color::Indexed(102),
            tar_exclude_button_selected_bg: Color::Indexed(60),
            tar_exclude_button_selected_text: Color::Indexed(195),

            // === Git Log Diff ===
            git_log_diff_title: Color::Indexed(195),
            git_log_diff_border: Color::Indexed(146),
            git_log_diff_bg: Color::Indexed(235),
            git_log_diff_message_text: Color::Indexed(188),
            git_log_diff_entry_text: Color::Indexed(188),
            git_log_diff_selected_text: Color::Indexed(108),
            git_log_diff_cursor_text: Color::Indexed(195),
            git_log_diff_cursor_bg: Color::Indexed(60),
            git_log_diff_button_text: Color::Indexed(102),
            git_log_diff_button_selected_text: Color::Indexed(195),
            git_log_diff_button_selected_bg: Color::Indexed(60),
            git_log_diff_button_disabled_text: Color::Indexed(239),
            git_log_diff_scroll_info: Color::Indexed(102),
            remote_bookmark_text: Color::Indexed(108),
            remote_connect_field_label: Color::Indexed(145),
            remote_connect_field_value: Color::Indexed(188),
            remote_connect_field_selected_bg: Color::Indexed(60),
        };

        let confirm_dialog = ConfirmDialogColors {
            bg: Color::Indexed(236),
            border: Color::Indexed(146),
            title: Color::Indexed(195),
            message_text: Color::Indexed(188),
            button_text: Color::Indexed(102),
            button_selected_bg: Color::Indexed(60),
            button_selected_text: Color::Indexed(195),
        };

        let settings = SettingsColors {
            bg: Color::Indexed(235),
            border: Color::Indexed(146),
            title: Color::Indexed(195),
            label_text: Color::Indexed(188),
            prompt: Color::Indexed(110),
            value_text: Color::Indexed(234),
            value_bg: Color::Indexed(146),
            help_key: Color::Indexed(146),
            help_text: Color::Indexed(102),
        };

        let editor = EditorColors {
            bg: Color::Indexed(234),
            border: Color::Indexed(146),
            header_bg: Color::Indexed(235),
            header_text: Color::Indexed(145),
            header_info: Color::Indexed(102),
            line_number: Color::Indexed(239),
            text: Color::Indexed(188),
            cursor: Color::Indexed(146),
            selection_bg: Color::Indexed(60),
            selection_text: Color::Indexed(195),
            match_bg: Color::Indexed(95),
            match_current_bg: Color::Indexed(132),
            bracket_match: Color::Indexed(110),
            modified_mark: Color::Indexed(174),
            footer_bg: Color::Indexed(235),
            footer_key: Color::Indexed(146),
            footer_text: Color::Indexed(102),
            find_input_text: Color::Indexed(188),
            find_option: Color::Indexed(102),
            find_option_active: Color::Indexed(110),
            wrap_indicator: Color::Indexed(239),
            remote_path_text: Color::Indexed(214),
        };

        let syntax = SyntaxColors {
            keyword: Color::Indexed(139),
            type_name: Color::Indexed(110),
            string: Color::Indexed(108),
            number: Color::Indexed(180),
            comment: Color::Indexed(59),
            operator: Color::Indexed(188),
            function: Color::Indexed(222),
            macro_name: Color::Indexed(139),
            attribute: Color::Indexed(102),
            variable: Color::Indexed(188),
            constant: Color::Indexed(174),
            bracket: Color::Indexed(145),
            normal: Color::Indexed(188),
        };

        let viewer = ViewerColors {
            bg: Color::Indexed(234),
            border: Color::Indexed(146),
            header_text: Color::Indexed(145),
            line_number: Color::Indexed(239),
            text: Color::Indexed(188),
            bookmark_indicator: Color::Indexed(110),
            search_input_text: Color::Indexed(188),
            search_cursor_fg: Color::Indexed(234),
            search_cursor_bg: Color::Indexed(146),
            search_match_current_bg: Color::Indexed(60),
            search_match_current_fg: Color::Indexed(195),
            search_match_other_bg: Color::Indexed(239),
            search_match_other_fg: Color::Indexed(188),
            search_info: Color::Indexed(102),
            hex_offset: Color::Indexed(102),
            hex_bytes: Color::Indexed(188),
            hex_ascii: Color::Indexed(195),
            wrap_indicator: Color::Indexed(239),
            footer_key: Color::Indexed(146),
            footer_text: Color::Indexed(102),
        };

        let process_manager = ProcessManagerColors {
            bg: Color::Indexed(234),
            border: Color::Indexed(146),
            header_text: Color::Indexed(145),
            column_header: Color::Indexed(110),
            text: Color::Indexed(188),
            selected_bg: Color::Indexed(60),
            selected_text: Color::Indexed(195),
            cpu_high: Color::Indexed(167),
            mem_high: Color::Indexed(167),
            confirm_text: Color::Indexed(174),
            footer_key: Color::Indexed(146),
            footer_text: Color::Indexed(102),
        };

        let ai_screen = AIScreenColors {
            bg: Color::Indexed(234),
            history_border: Color::Indexed(146),
            history_title: Color::Indexed(195),
            history_placeholder: Color::Indexed(102),
            history_scroll_info: Color::Indexed(102),
            user_prefix: Color::Indexed(110),
            assistant_prefix: Color::Indexed(139),
            error_prefix: Color::Indexed(167),
            system_prefix: Color::Indexed(102),
            message_text: Color::Indexed(188),
            input_border: Color::Indexed(146),
            input_prompt: Color::Indexed(110),
            input_text: Color::Indexed(188),
            input_cursor_fg: Color::Indexed(234),
            input_cursor_bg: Color::Indexed(146),
            input_placeholder: Color::Indexed(102),
            processing_spinner: Color::Indexed(110),
            processing_text: Color::Indexed(102),
            error_text: Color::Indexed(167),
            tool_use_prefix: Color::Indexed(180),
            tool_use_name: Color::Indexed(110),
            tool_use_input: Color::Indexed(188),
            tool_result_prefix: Color::Indexed(108),
            tool_result_text: Color::Indexed(188),
            footer_key: Color::Indexed(146),
            footer_text: Color::Indexed(102),
        };

        let system_info = SystemInfoColors {
            bg: Color::Indexed(234),
            border: Color::Indexed(146),
            section_title: Color::Indexed(110),
            label: Color::Indexed(188),
            value: Color::Indexed(188),
            bar_fill: Color::Indexed(108),
            bar_empty: Color::Indexed(239),
            usage_low: Color::Indexed(108),
            usage_medium: Color::Indexed(180),
            usage_high: Color::Indexed(167),
            tab_active: Color::Indexed(195),
            disk_header: Color::Indexed(110),
            disk_text: Color::Indexed(188),
            selected_bg: Color::Indexed(60),
            selected_text: Color::Indexed(195),
            footer_key: Color::Indexed(146),
            footer_text: Color::Indexed(102),
        };

        let search_result = SearchResultColors {
            bg: Color::Indexed(234),
            border: Color::Indexed(146),
            header_text: Color::Indexed(145),
            column_header: Color::Indexed(110),
            column_header_dim: Color::Indexed(102),
            directory_text: Color::Indexed(195),
            file_text: Color::Indexed(188),
            selected_bg: Color::Indexed(60),
            selected_text: Color::Indexed(195),
            match_highlight: Color::Indexed(174),
            path_text: Color::Indexed(102),
            footer_key: Color::Indexed(146),
            footer_text: Color::Indexed(102),
        };

        let image_viewer = ImageViewerColors {
            bg: Color::Indexed(234),
            border: Color::Indexed(146),
            title_text: Color::Indexed(145),
            loading_spinner: Color::Indexed(110),
            loading_text: Color::Indexed(102),
            error_text: Color::Indexed(167),
            hint_text: Color::Indexed(102),
            footer_key: Color::Indexed(146),
            footer_text: Color::Indexed(102),
            footer_separator: Color::Indexed(102),
        };

        let file_info = FileInfoColors {
            bg: Color::Indexed(234),
            border: Color::Indexed(146),
            title: Color::Indexed(195),
            label: Color::Indexed(102),
            value: Color::Indexed(188),
            value_name: Color::Indexed(110),
            value_path: Color::Indexed(188),
            value_type: Color::Indexed(188),
            value_size: Color::Indexed(108),
            value_permission: Color::Indexed(188),
            value_owner: Color::Indexed(188),
            value_date: Color::Indexed(188),
            calculating_spinner: Color::Indexed(110),
            calculating_text: Color::Indexed(110),
            error_text: Color::Indexed(167),
            hint_text: Color::Indexed(102),
        };

        let help = HelpColors {
            bg: Color::Indexed(234),
            border: Color::Indexed(146),
            title: Color::Indexed(195),
            section_title: Color::Indexed(110),
            section_decorator: Color::Indexed(239),
            key: Color::Indexed(146),
            key_highlight: Color::Indexed(174),
            description: Color::Indexed(188),
            hint_text: Color::Indexed(102),
        };

        let advanced_search = AdvancedSearchColors {
            bg: Color::Indexed(234),
            border: Color::Indexed(146),
            title: Color::Indexed(195),
            label: Color::Indexed(188),
            input_text: Color::Indexed(188),
            input_cursor: Color::Indexed(146),
            field_bracket: Color::Indexed(110),
            checkbox_checked: Color::Indexed(108),
            checkbox_unchecked: Color::Indexed(102),
            button_text: Color::Indexed(102),
            button_selected_bg: Color::Indexed(60),
            button_selected_text: Color::Indexed(195),
            footer_key: Color::Indexed(146),
            footer_text: Color::Indexed(102),
        };

        let diff = DiffColors {
            bg: Color::Indexed(234),
            border: Color::Indexed(102),
            header_text: Color::Indexed(188),
            header_label: Color::Indexed(146),
            column_header_bg: Color::Indexed(235),
            column_header_text: Color::Indexed(188),
            same_text: Color::Indexed(188),
            modified_text: Color::Indexed(174),
            modified_bg: Color::Indexed(95),
            left_only_text: Color::Indexed(73),
            left_only_bg: Color::Indexed(23),
            right_only_text: Color::Indexed(73),
            right_only_bg: Color::Indexed(23),
            empty_bg: Color::Indexed(235),
            dir_same_text: Color::Indexed(110),
            dir_modified_text: Color::Indexed(174),
            cursor_bg: Color::Indexed(60),
            cursor_text: Color::Indexed(195),
            marked_text: Color::Indexed(174),
            size_text: Color::Indexed(102),
            date_text: Color::Indexed(102),
            status_bar_bg: Color::Indexed(235),
            status_bar_text: Color::Indexed(188),
            filter_label: Color::Indexed(146),
            stats_text: Color::Indexed(188),
            footer_key: Color::Indexed(146),
            footer_text: Color::Indexed(102),
            panel_selected_border: Color::Indexed(174),
            progress_spinner: Color::Indexed(146),
            progress_bar_fill: Color::Indexed(146),
            progress_bar_empty: Color::Indexed(102),
            progress_percent_text: Color::Indexed(188),
            progress_value_text: Color::Indexed(188),
            progress_hint_text: Color::Indexed(102),
        };

        let diff_file_view = DiffFileViewColors {
            bg: Color::Indexed(234),
            border: Color::Indexed(146),
            header_text: Color::Indexed(188),
            line_number: Color::Indexed(239),
            same_text: Color::Indexed(188),
            modified_text: Color::Indexed(217),
            modified_bg: Color::Indexed(95),
            left_only_text: Color::Indexed(116),
            left_only_bg: Color::Indexed(23),
            right_only_text: Color::Indexed(116),
            right_only_bg: Color::Indexed(23),
            empty_bg: Color::Indexed(235),
            inline_change_bg: Color::Indexed(132),
            inline_change_text: Color::Indexed(217),
            status_bar_bg: Color::Indexed(235),
            status_bar_text: Color::Indexed(188),
            footer_key: Color::Indexed(146),
            footer_text: Color::Indexed(102),
        };

        let git_screen = GitScreenColors {
            bg: Color::Indexed(234),
            border: Color::Indexed(102),
            header_branch: Color::Indexed(108),
            header_path: Color::Indexed(188),
            tab_active: Color::Indexed(110),
            tab_inactive: Color::Indexed(102),
            tab_bar_bg: Color::Indexed(235),
            file_staged: Color::Indexed(108),
            file_modified: Color::Indexed(180),
            file_untracked: Color::Indexed(174),
            file_deleted: Color::Indexed(167),
            selected_bg: Color::Indexed(239),
            selected_text: Color::Indexed(195),
            footer_key: Color::Indexed(146),
            footer_text: Color::Indexed(102),
            commit_input_border: Color::Indexed(102),
            commit_input_text: Color::Indexed(188),
            log_hash: Color::Indexed(146),
            log_message: Color::Indexed(188),
            log_author: Color::Indexed(110),
            log_date: Color::Indexed(102),
            branch_current: Color::Indexed(108),
            branch_normal: Color::Indexed(188),
            diff_add: Color::Indexed(108),
            diff_remove: Color::Indexed(174),
            diff_header: Color::Indexed(110),
        };

        let dedup_screen = DedupScreenColors {
            bg: Color::Indexed(234),
            border: Color::Indexed(102),
            title: Color::Indexed(110),
            phase_text: Color::Indexed(108),
            stats_text: Color::Indexed(188),
            progress_bar_fill: Color::Indexed(108),
            progress_bar_empty: Color::Indexed(236),
            progress_text: Color::Indexed(188),
            log_text: Color::Indexed(188),
            log_text_alt: Color::Indexed(144),
            log_deleted: Color::Indexed(174),
            log_error: Color::Indexed(167),
            footer_key: Color::Indexed(146),
            footer_text: Color::Indexed(102),
        };

        Self {
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

    // ═══════════════════════════════════════════════════════════════════════════
    // 스타일 헬퍼 메서드
    // ═══════════════════════════════════════════════════════════════════════════

    pub fn normal_style(&self) -> Style {
        Style::default().fg(self.panel.file_text)
    }

    pub fn dim_style(&self) -> Style {
        Style::default().fg(self.palette.fg_dim)
    }

    pub fn selected_style(&self) -> Style {
        Style::default()
            .fg(self.panel.selected_text)
            .bg(self.panel.selected_bg)
    }

    pub fn directory_style(&self) -> Style {
        Style::default()
            .fg(self.panel.directory_text)
            .add_modifier(Modifier::BOLD)
    }

    pub fn symlink_style(&self) -> Style {
        Style::default().fg(self.panel.symlink_text)
    }

    pub fn header_style(&self) -> Style {
        Style::default()
            .fg(self.panel.header_text)
            .add_modifier(Modifier::BOLD)
    }

    pub fn border_style(&self, active: bool) -> Style {
        if active {
            Style::default().fg(self.panel.border_active)
        } else {
            Style::default().fg(self.panel.border)
        }
    }

    pub fn warning_style(&self) -> Style {
        Style::default()
            .fg(self.state.warning)
            .add_modifier(Modifier::BOLD)
    }

    pub fn error_style(&self) -> Style {
        Style::default().fg(self.state.error)
    }

    pub fn success_style(&self) -> Style {
        Style::default().fg(self.state.success)
    }

    pub fn marked_style(&self) -> Style {
        Style::default().fg(self.panel.marked_text)
    }

    pub fn status_bar_style(&self) -> Style {
        Style::default()
            .fg(self.status_bar.text)
            .bg(self.status_bar.bg)
    }

    pub fn info_style(&self) -> Style {
        Style::default().fg(self.state.info)
    }

    // ═══════════════════════════════════════════════════════════════════════════
    // JSON 변환
    // ═══════════════════════════════════════════════════════════════════════════

    /// Color::Indexed(n)에서 n을 추출
    fn color_index(color: Color) -> u8 {
        match color {
            Color::Indexed(n) => n,
            _ => 0,
        }
    }

    /// 테마 이름 반환
    pub fn name(&self) -> &str {
        // 테마 구분: 색상 조합으로 판단
        let bg = Self::color_index(self.palette.bg);
        let accent = Self::color_index(self.palette.accent);
        let shortcut = Self::color_index(self.palette.shortcut);

        // dawn of coding: bg=234, accent=110, shortcut=146
        if bg == 234 && accent == 110 && shortcut == 146 {
            "dawn_of_coding"
        } else if bg >= 250 {
            "light"
        } else {
            "dark"
        }
    }

    /// Theme을 JSON 문자열로 변환 (설정 파일 저장용)
    pub fn to_json(&self) -> String {
        let ci = Self::color_index;
        format!(
            r#"{{
  "name": "{}",

  "__palette__": "=== 기본 팔레트: 앱 전체에서 참조되는 시맨틱 컬러 토큰. 개별 컴포넌트 색상의 기반이 되는 핵심 색상 정의 ===",
  "palette": {{
    "__bg__": "앱 전체의 기본 배경색. 모든 UI 요소가 이 배경 위에 렌더링됨. fg, fg_dim, fg_strong 텍스트들과 충분한 명도 대비 필요. panel.bg, editor.bg, dialog.bg 등 각 컴포넌트 배경색의 기준점",
    "bg": {},
    "__bg_alt__": "bg와 구분되는 대체 배경색. 헤더, 푸터, 상태바 등 영역 구분이 필요한 곳에 사용. bg와 미세한 명도 차이로 시각적 계층 형성. status_bar.bg, editor.header_bg, editor.footer_bg 등에서 참조",
    "bg_alt": {},
    "__fg__": "기본 텍스트 색상. bg 위에 표시되는 일반 본문 텍스트. 가독성을 위해 bg와 높은 대비 필요. panel.file_text, editor.text, dialog.text 등 대부분의 텍스트 색상 기준",
    "fg": {},
    "__fg_dim__": "보조 텍스트 색상. fg보다 낮은 시각적 우선순위. 날짜, 크기, 힌트, 비활성 항목 등 부가 정보에 사용. panel.size_text, panel.date_text, status_bar.text_dim 등에서 참조. bg와의 대비는 fg보다 낮지만 여전히 읽을 수 있어야 함",
    "fg_dim": {},
    "__fg_strong__": "강조 텍스트 색상. fg보다 높은 시각적 우선순위. 제목, 디렉토리명, 중요 정보에 사용. panel.directory_text, header.title, dialog.title 등에서 참조. bg와 fg 사이에서 가장 높은 대비",
    "fg_strong": {},
    "__fg_inverse__": "반전 배경 위의 텍스트 색상. selected_bg, button_selected_bg 등 강조 배경 위에 표시될 때 사용. 해당 배경색들과 충분한 대비 필요. panel.selected_text, dialog.button_selected_text 등에서 참조",
    "fg_inverse": {},
    "__accent__": "정보성 강조색. 링크, 컬럼 헤더, 활성 상태 표시 등 사용자 주의를 끄는 요소에 사용. 중립적이면서 눈에 띄는 색상. process_manager.column_header, search_result.column_header 등에서 참조",
    "accent": {},
    "__shortcut__": "단축키 표시 색상. 키보드 단축키(Ctrl+C, Space 등)를 본문 텍스트와 구분하여 표시. function_bar.key, editor.footer_key, dialog.help_key_text 등에서 참조. fg와 구분되면서 bg 위에서 잘 보여야 함",
    "shortcut": {},
    "__positive__": "긍정/성공 상태 색상. 작업 완료, 체크마크, 진행바 채움 등에 사용. state.success와 동일하거나 유사. system_info.bar_fill, advanced_search.checkbox_checked 등에서 참조",
    "positive": {},
    "__highlight__": "강조/경고 색상. 마킹된 파일, 검색 매치, 주의가 필요한 상태 표시. panel.marked_text, editor.match_bg, search_result.match_highlight 등에서 참조. bg 위에서 즉시 눈에 띄어야 함",
    "highlight": {}
  }},

  "__state__": "=== 상태 색상: 작업 결과와 시스템 상태를 사용자에게 전달하는 시맨틱 컬러 ===",
  "state": {{
    "__success__": "성공 상태 표시. 파일 저장 완료, 복사 성공 등 긍정적 결과. 일반적으로 녹색 계열. palette.positive와 동일하거나 유사하게 설정",
    "success": {},
    "__warning__": "경고 상태 표시. 덮어쓰기 확인, 용량 부족 경고 등 주의가 필요한 상황. 일반적으로 노란색/주황색 계열. error보다는 덜 심각한 상황에 사용",
    "warning": {},
    "__error__": "에러 상태 표시. 작업 실패, 파일 접근 오류 등 문제 상황. 일반적으로 빨간색 계열. 사용자가 즉시 인지해야 하는 심각한 상황에 사용",
    "error": {},
    "__info__": "정보 상태 표시. 일반 알림, 진행 상황 안내 등 중립적 정보. 일반적으로 파란색 계열. palette.accent와 유사하게 설정 가능",
    "info": {}
  }},

  "__panel__": "=== 파일 패널: 메인 화면의 좌/우 듀얼 패널. 파일 탐색의 핵심 UI로 가장 많은 시간 노출되는 영역 ===",
  "panel": {{
    "__bg__": "파일 목록 패널의 기본 배경색. 좌우 듀얼 패널에서 파일/폴더 목록이 표시되는 메인 영역. 이 위에 file_text, directory_text, selected_bg가 렌더링됨. file_text와 충분한 명도 대비 필요. 장시간 사용해도 눈의 피로가 적은 색상 권장",
    "bg": {},
    "__border__": "비활성 패널의 테두리. 좌우 패널 중 현재 포커스가 없는 쪽. border_active보다 낮은 시각적 강조. bg와 구분되면서 border_active와도 명확히 구분되어야 함",
    "border": {},
    "__border_active__": "활성 패널의 테두리. 현재 키보드 포커스가 있는 패널. border보다 높은 시각적 강조로 사용자가 현재 작업 중인 패널을 즉시 인식할 수 있어야 함",
    "border_active": {},
    "__header_bg__": "비활성 패널 헤더의 배경색. 현재 경로를 표시하는 상단 영역. header_text가 이 위에 표시됨. bg와 미세하게 구분되어 헤더 영역임을 인식할 수 있어야 함. header_bg_active보다 낮은 강조",
    "header_bg": {},
    "__header_bg_active__": "활성 패널 헤더의 배경색. header_bg보다 높은 시각적 강조. header_text_active가 이 위에 표시됨. 사용자가 현재 작업 중인 패널의 경로를 쉽게 확인할 수 있어야 함",
    "header_bg_active": {},
    "__header_text__": "비활성 패널 헤더의 경로 텍스트. header_bg 위에 표시됨. header_text_active보다 낮은 시각적 강조. 읽을 수 있지만 활성 패널보다 덜 눈에 띄어야 함",
    "header_text": {},
    "__header_text_active__": "활성 패널 헤더의 경로 텍스트. header_bg_active 위에 표시됨. header_text보다 높은 시각적 강조. 현재 작업 디렉토리를 명확히 인식할 수 있어야 함",
    "header_text_active": {},
    "__file_text__": "일반 파일명 텍스트 색상. bg 위에 표시되는 파일 목록의 기본 텍스트. directory_text와 구분되어 파일/폴더를 시각적으로 구별할 수 있어야 함. 가장 많이 보이는 텍스트로 bg와 충분한 대비 필수",
    "file_text": {},
    "__directory_text__": "디렉토리(폴더)명 텍스트 색상. bg 위에 표시됨. file_text와 다른 색상으로 폴더를 즉시 구별할 수 있어야 함. 일반적으로 file_text보다 강조된 색상 또는 다른 색조 사용",
    "directory_text": {},
    "__symlink_text__": "심볼릭 링크 텍스트 색상. bg 위에 표시됨. file_text, directory_text와 다른 색상으로 링크임을 즉시 구별할 수 있어야 함. 일반적으로 cyan 계열 색상 사용",
    "symlink_text": {},
    "__selected_bg__": "현재 커서가 위치한 항목의 배경 하이라이트. 사용자의 현재 포커스를 명확히 표시하는 핵심 요소. bg와 확연히 구분되어야 함. 그 위에 selected_text가 표시됨. marked_text와도 시각적으로 구분 필요",
    "selected_bg": {},
    "__selected_text__": "선택된 항목의 텍스트 색상. selected_bg 위에 표시되므로 배경색과 대비되는 어두운 색이어야 함",
    "selected_text": {},
    "__marked_text__": "Space키로 마킹된 파일의 텍스트 색상. 다중 선택된 파일들을 표시. bg 위에서 file_text, directory_text와 명확히 구분되어야 함. selected_bg와 겹칠 수 있으므로 selected_bg 위에서도 보여야 함",
    "marked_text": {},
    "__size_text__": "파일 크기 컬럼의 텍스트 색상. bg 위에 표시됨. file_text보다 낮은 시각적 우선순위. 보조 정보로서 읽을 수 있지만 파일명보다 덜 눈에 띄어야 함. palette.fg_dim 참조",
    "size_text": {},
    "__date_text__": "수정일 컬럼의 텍스트 색상. bg 위에 표시됨. size_text와 유사한 시각적 우선순위. 보조 정보로서 파일명보다 덜 강조됨. palette.fg_dim 참조",
    "date_text": {},
    "__remote_indicator__": "원격 패널 [SSH] 인디케이터 색상. 패널 헤더에서 원격 연결 상태를 나타내는 텍스트 색상",
    "remote_indicator": {}
  }},

  "__header__": "=== 앱 헤더: 화면 최상단의 앱 제목 및 브랜딩 영역. 항상 표시되는 고정 요소 ===",
  "header": {{
    "__bg__": "앱 헤더의 배경색. 화면 최상단에 위치하며 앱 제목이 표시되는 영역. panel.bg와 구분되어 헤더 영역임을 인식할 수 있어야 함. text, title이 이 위에 표시됨",
    "bg": {},
    "__text__": "헤더의 일반 텍스트 색상. bg 위에 표시되는 부가 정보. title보다 낮은 시각적 우선순위",
    "text": {},
    "__title__": "앱 제목(CKD) 텍스트 색상. bg 위에 표시되는 앱 브랜드명. text보다 높은 시각적 강조. 앱의 아이덴티티를 나타내는 요소",
    "title": {}
  }},

  "__status_bar__": "=== 상태 표시줄: 현재 선택된 파일 정보, 디스크 사용량 등 컨텍스트 정보를 표시하는 하단 영역 ===",
  "status_bar": {{
    "__bg__": "상태 표시줄의 배경색. 화면 하단에 위치하며 현재 상태 정보가 표시됨. panel.bg와 구분되어 상태바 영역임을 인식할 수 있어야 함. palette.bg_alt 참조 가능. text, text_dim이 이 위에 표시됨",
    "bg": {},
    "__text__": "상태바의 주요 정보 텍스트. bg 위에 표시됨. 현재 선택된 파일명, 파일 타입 등 중요 정보. text_dim보다 높은 시각적 우선순위",
    "text": {},
    "__text_dim__": "상태바의 보조 정보 텍스트. bg 위에 표시됨. 파일 수, 총 용량, 마킹된 파일 수 등 부가 정보. text보다 낮은 시각적 강조. palette.fg_dim 참조",
    "text_dim": {}
  }},

  "__function_bar__": "=== 단축키 바: 화면 최하단에서 사용 가능한 단축키를 안내 ===",
  "function_bar": {{
    "__bg__": "단축키 바의 배경색. 화면 최하단에 위치하며 단축키 안내가 표시됨. panel.bg, status_bar.bg와 구분 가능해야 함. key, label이 이 위에 표시됨",
    "bg": {},
    "__key__": "단축키 텍스트 색상 (h, e, x 등). bg 위에 표시됨. label과 쌍을 이루며 단축키 부분을 강조. label보다 높은 시각적 우선순위로 키를 먼저 인식할 수 있어야 함. palette.shortcut 참조",
    "key": {},
    "__label__": "단축키 설명 텍스트 (Help, Menu 등). bg 위에 표시됨. key 옆에 위치하여 해당 키의 기능 설명. key보다 낮은 시각적 강조",
    "label": {}
  }},

  "__message__": "=== 메시지: 작업 결과나 에러를 일시적으로 표시하는 알림 영역. 상태바 위치에 오버레이됨 ===",
  "message": {{
    "__bg__": "메시지 영역의 배경색. 작업 완료/실패 시 일시적으로 표시되는 알림의 배경. text가 이 위에 표시됨. 기본 UI와 구분되어 메시지임을 인식할 수 있어야 함",
    "bg": {},
    "__text__": "메시지 텍스트 색상. bg 위에 표시됨. 작업 결과, 에러 메시지 등. 사용자가 즉시 인지해야 하는 정보로 bg와 높은 대비 필요. state.error, state.success 등과 연계 가능",
    "text": {}
  }},

  "__dialog__": "=== 다이얼로그: 복사/이동/이름변경/검색 등 사용자 입력을 받는 모달 대화상자. 메인 화면 위에 오버레이로 표시됨 ===",
  "dialog": {{
    "__bg__": "다이얼로그의 기본 배경색. 메인 화면(panel) 위에 오버레이되는 모달 창의 배경. border로 둘러싸이며, 내부의 모든 요소(title, text, input, button 등)가 이 위에 표시됨. panel.bg와 구분되어 모달임을 인식할 수 있어야 함",
    "bg": {},
    "__border__": "다이얼로그의 테두리 색상. bg를 둘러싸며 다이얼로그 영역을 정의. 배경(panel.bg)과 다이얼로그(bg) 사이의 경계 역할",
    "border": {},
    "__title__": "다이얼로그 제목 텍스트 색상. bg 위 상단에 표시되는 대화상자 제목(Copy, Move, Rename 등). text보다 높은 시각적 강조로 다이얼로그의 목적을 명확히 전달",
    "title": {},
    "__text__": "다이얼로그의 일반 텍스트 색상. bg 위에 표시되는 설명, 안내 문구. text_dim보다 높은 시각적 우선순위",
    "text": {},
    "__text_dim__": "다이얼로그의 보조 텍스트 색상. bg 위에 표시되는 부가 설명, 힌트. text보다 낮은 시각적 강조",
    "text_dim": {},
    "__message_text__": "다이얼로그 내 안내 메시지 텍스트. bg 위에 표시됨. 'Copy file to:' 등 사용자에게 현재 작업을 안내하는 문구",
    "message_text": {},
    "__input_text__": "입력 필드의 텍스트 색상. bg 위에 표시되는 사용자 입력값. 사용자가 타이핑한 경로나 파일명이 명확히 보여야 함",
    "input_text": {},
    "__input_cursor_fg__": "입력 커서의 전경색. 커서 위치의 문자 색상. input_cursor_bg와 함께 현재 입력 위치를 표시",
    "input_cursor_fg": {},
    "__input_cursor_bg__": "입력 커서의 배경색. bg와 확연히 구분되어 현재 커서 위치를 명확히 표시. input_cursor_fg가 이 위에 표시됨",
    "input_cursor_bg": {},
    "__input_prompt__": "입력 프롬프트(>> 또는 >) 색상. bg 위에 표시되며 입력 필드의 시작을 나타냄. input_text와 구분되는 색상으로 프롬프트임을 인식",
    "input_prompt": {},
    "__button_text__": "비선택 버튼의 텍스트 색상. bg 위에 표시됨. button_selected_text보다 낮은 시각적 강조. OK, Cancel 등 선택 가능한 버튼 중 현재 포커스가 없는 버튼",
    "button_text": {},
    "__button_selected_bg__": "선택된 버튼의 배경색. 현재 포커스가 있는 버튼의 하이라이트. bg와 확연히 구분되어야 함. button_selected_text가 이 위에 표시됨. panel.selected_bg와 유사한 역할",
    "button_selected_bg": {},
    "__button_selected_text__": "선택된 버튼의 텍스트 색상. button_selected_bg 위에 표시됨. 선택된 버튼의 레이블이 명확히 보여야 함. palette.fg_inverse 참조",
    "button_selected_text": {},
    "__autocomplete_bg__": "자동완성 드롭다운 목록의 배경색. 입력 필드 아래에 표시되는 경로/파일 제안 목록. bg와 구분되거나 동일할 수 있음. autocomplete_text, autocomplete_selected_bg가 이 위에 표시됨",
    "autocomplete_bg": {},
    "__autocomplete_text__": "자동완성 목록의 파일 항목 텍스트. autocomplete_bg 위에 표시됨. autocomplete_directory_text와 구분되어 파일/폴더를 구별할 수 있어야 함",
    "autocomplete_text": {},
    "__autocomplete_directory_text__": "자동완성 목록의 디렉토리 항목 텍스트. autocomplete_bg 위에 표시됨. autocomplete_text와 다른 색상으로 폴더임을 즉시 인식. panel.directory_text와 유사한 역할",
    "autocomplete_directory_text": {},
    "__autocomplete_selected_bg__": "자동완성 목록에서 선택된 항목의 배경색. autocomplete_bg 위에 표시되며 현재 포커스를 나타냄. autocomplete_selected_text가 이 위에 표시됨",
    "autocomplete_selected_bg": {},
    "__autocomplete_selected_text__": "자동완성 목록에서 선택된 항목의 텍스트. autocomplete_selected_bg 위에 표시됨. 선택된 제안 항목이 명확히 보여야 함",
    "autocomplete_selected_text": {},
    "__autocomplete_scroll_info__": "자동완성 목록의 스크롤 위치 정보([1/10] 형식). autocomplete_bg 위에 표시됨. 현재 목록 위치를 안내하는 보조 정보",
    "autocomplete_scroll_info": {},
    "__preview_suffix_text__": "경로 미리보기의 접미사 텍스트. 입력 중인 경로의 자동완성 힌트. input_text와 구분되어 실제 입력과 제안을 구별",
    "preview_suffix_text": {},
    "__help_key_text__": "다이얼로그 하단 도움말의 단축키 부분. bg 위에 표시됨. Tab, Enter 등 사용 가능한 키 안내. help_label_text와 쌍을 이루며 키 부분을 강조",
    "help_key_text": {},
    "__help_label_text__": "다이얼로그 하단 도움말의 설명 부분. bg 위에 표시됨. help_key_text 옆에 위치하여 해당 키의 기능 설명",
    "help_label_text": {},
    "__progress_label_text__": "진행률 다이얼로그의 레이블 텍스트(File:, Total: 등). bg 위에 표시됨. 현재 진행 중인 작업의 항목명. progress_value_text와 쌍을 이룸",
    "progress_label_text": {},
    "__progress_value_text__": "진행률 다이얼로그의 값 텍스트. bg 위에 표시됨. 현재 처리 중인 파일명, 복사된 바이트 수 등. progress_label_text보다 강조될 수 있음",
    "progress_value_text": {},
    "__progress_bar_fill__": "진행률 바의 채워진 부분. 완료된 진행률을 시각적으로 표시. progress_bar_empty와 나란히 배치되어 전체 대비 완료량을 표현. palette.positive와 유사",
    "progress_bar_fill": {},
    "__progress_bar_empty__": "진행률 바의 빈 부분. 남은 진행률을 나타냄. progress_bar_fill과 함께 전체 진행 상황을 시각화",
    "progress_bar_empty": {},
    "__progress_percent_text__": "진행률 퍼센트 텍스트(45% 등). bg 위에 표시됨. 숫자로 정확한 진행률 표시",
    "progress_percent_text": {},
    "__conflict_filename_text__": "파일 충돌 다이얼로그에서 충돌 파일명 강조. bg 위에 표시됨. 어떤 파일이 충돌하는지 사용자가 즉시 인식할 수 있도록 강조. palette.highlight와 유사",
    "conflict_filename_text": {},
    "__conflict_count_text__": "충돌 다이얼로그의 진행 정보(1 of 3 conflicts). bg 위에 표시됨. 전체 충돌 수 대비 현재 위치를 안내",
    "conflict_count_text": {},
    "__conflict_shortcut_text__": "충돌 다이얼로그 버튼의 단축키 문자(O, S, A, l 등). 버튼 레이블 내에서 단축키를 강조. 빠른 키보드 조작을 안내",
    "conflict_shortcut_text": {},
    "__tar_exclude_title__": "압축 제외 확인 다이얼로그의 제목. tar_exclude_bg 위에 표시됨. dialog.title과 유사한 역할",
    "tar_exclude_title": {},
    "__tar_exclude_border__": "압축 제외 다이얼로그의 테두리. tar_exclude_bg를 둘러쌈. dialog.border와 유사한 역할",
    "tar_exclude_border": {},
    "__tar_exclude_bg__": "압축 제외 다이얼로그의 배경. dialog.bg와 동일하거나 유사",
    "tar_exclude_bg": {},
    "__tar_exclude_message_text__": "압축 제외 다이얼로그의 안내 메시지. tar_exclude_bg 위에 표시됨",
    "tar_exclude_message_text": {},
    "__tar_exclude_path_text__": "제외될 경로 목록의 텍스트. tar_exclude_bg 위에 표시됨. 어떤 파일/폴더가 제외되는지 명확히 표시. 경로이므로 주의를 끄는 색상 사용 가능",
    "tar_exclude_path_text": {},
    "__tar_exclude_scroll_info__": "제외 목록의 스크롤 정보. tar_exclude_bg 위에 표시됨. 목록이 길 때 현재 위치 안내",
    "tar_exclude_scroll_info": {},
    "__tar_exclude_button_text__": "압축 제외 다이얼로그의 비선택 버튼 텍스트. dialog.button_text와 유사",
    "tar_exclude_button_text": {},
    "__tar_exclude_button_selected_bg__": "압축 제외 다이얼로그의 선택된 버튼 배경. dialog.button_selected_bg와 유사",
    "tar_exclude_button_selected_bg": {},
    "__tar_exclude_button_selected_text__": "압축 제외 다이얼로그의 선택된 버튼 텍스트. tar_exclude_button_selected_bg 위에 표시됨",
    "tar_exclude_button_selected_text": {},
    "__git_log_diff_title__": "Git Log Diff 다이얼로그의 제목. git_log_diff_bg 위에 표시됨",
    "git_log_diff_title": {},
    "__git_log_diff_border__": "Git Log Diff 다이얼로그의 테두리. git_log_diff_bg를 둘러쌈",
    "git_log_diff_border": {},
    "__git_log_diff_bg__": "Git Log Diff 다이얼로그의 배경색",
    "git_log_diff_bg": {},
    "__git_log_diff_message_text__": "Git Log Diff 다이얼로그의 안내 메시지 텍스트. git_log_diff_bg 위에 표시됨",
    "git_log_diff_message_text": {},
    "__git_log_diff_entry_text__": "Git Log Diff 다이얼로그의 커밋 항목 텍스트. git_log_diff_bg 위에 표시됨",
    "git_log_diff_entry_text": {},
    "__git_log_diff_selected_text__": "Git Log Diff 다이얼로그에서 Space로 선택(체크)된 커밋 텍스트. git_log_diff_bg 위에 표시됨",
    "git_log_diff_selected_text": {},
    "__git_log_diff_cursor_text__": "Git Log Diff 다이얼로그에서 현재 커서 위치의 텍스트. git_log_diff_cursor_bg 위에 표시됨",
    "git_log_diff_cursor_text": {},
    "__git_log_diff_cursor_bg__": "Git Log Diff 다이얼로그에서 현재 커서 위치의 배경색",
    "git_log_diff_cursor_bg": {},
    "__git_log_diff_button_text__": "Git Log Diff 다이얼로그의 비선택 버튼 텍스트",
    "git_log_diff_button_text": {},
    "__git_log_diff_button_selected_text__": "Git Log Diff 다이얼로그의 선택된 버튼 텍스트. git_log_diff_button_selected_bg 위에 표시됨",
    "git_log_diff_button_selected_text": {},
    "__git_log_diff_button_selected_bg__": "Git Log Diff 다이얼로그의 선택된 버튼 배경색",
    "git_log_diff_button_selected_bg": {},
    "__git_log_diff_button_disabled_text__": "Git Log Diff 다이얼로그의 비활성(2개 미선택) 버튼 텍스트",
    "git_log_diff_button_disabled_text": {},
    "__git_log_diff_scroll_info__": "Git Log Diff 다이얼로그의 스크롤 정보. git_log_diff_bg 위에 표시됨",
    "git_log_diff_scroll_info": {},
    "__remote_bookmark_text__": "북마크 목록 내 원격 프로필 텍스트 색상. 로컬 북마크와 시각적으로 구분하기 위한 색상",
    "remote_bookmark_text": {},
    "__remote_connect_field_label__": "원격 연결 다이얼로그의 필드 레이블 (Host:, Port:, User: 등)",
    "remote_connect_field_label": {},
    "__remote_connect_field_value__": "원격 연결 다이얼로그의 필드 값 텍스트",
    "remote_connect_field_value": {},
    "__remote_connect_field_selected_bg__": "원격 연결 다이얼로그에서 선택된 필드의 배경색",
    "remote_connect_field_selected_bg": {}
  }},

  "__confirm_dialog__": "=== 확인 다이얼로그: 파일 삭제, 덮어쓰기, 대용량 파일 열기 등 사용자 확인이 필요한 작업의 모달 ===",
  "confirm_dialog": {{
    "__bg__": "확인 다이얼로그의 배경색. 위험하거나 중요한 작업 전 사용자 확인을 받는 모달 창. dialog.bg와 동일하거나 유사. title, message_text, 버튼들이 이 위에 표시됨",
    "bg": {},
    "__border__": "확인 다이얼로그의 테두리. bg를 둘러싸며 모달 영역을 정의. dialog.border와 유사",
    "border": {},
    "__title__": "확인 다이얼로그의 제목(Delete, Confirm 등). bg 위에 표시됨. 어떤 확인을 요청하는지 명확히 전달. 삭제 등 위험한 작업일 경우 주의를 끄는 색상 가능",
    "title": {},
    "__message_text__": "확인 메시지 본문. bg 위에 표시됨. 'Are you sure you want to delete?' 등 상세 설명",
    "message_text": {},
    "__button_text__": "비선택 버튼의 텍스트(Yes/No, OK/Cancel). bg 위에 표시됨. button_selected_text보다 낮은 시각적 강조",
    "button_text": {},
    "__button_selected_bg__": "선택된 버튼의 배경색. 현재 포커스가 있는 버튼 하이라이트. button_selected_text가 이 위에 표시됨",
    "button_selected_bg": {},
    "__button_selected_text__": "선택된 버튼의 텍스트. button_selected_bg 위에 표시됨. 현재 선택될 옵션이 명확히 보여야 함",
    "button_selected_text": {}
  }},

  "__settings__": "=== 설정 다이얼로그: 테마 선택, 앱 설정 등을 변경하는 환경설정 화면 ===",
  "settings": {{
    "__bg__": "설정 다이얼로그의 배경색. 앱 설정을 변경하는 모달 창. dialog.bg와 유사. 설정 항목들과 도움말이 이 위에 표시됨",
    "bg": {},
    "__border__": "설정 다이얼로그의 테두리. bg를 둘러싸며 모달 영역을 정의",
    "border": {},
    "__title__": "설정 다이얼로그의 제목(Settings). bg 위에 표시됨",
    "title": {},
    "__label_text__": "설정 항목의 레이블(Theme:, Language: 등). bg 위에 표시됨. 각 설정 항목이 무엇인지 안내. value_text와 쌍을 이룸",
    "label_text": {},
    "__prompt__": "현재 선택된 설정 항목을 가리키는 프롬프트(>). bg 위에 표시됨. 현재 포커스가 어느 설정에 있는지 표시",
    "prompt": {},
    "__value_text__": "설정값 텍스트(light, dark 등). value_bg 위에 표시됨. 현재 선택된 값이 명확히 보여야 함",
    "value_text": {},
    "__value_bg__": "설정값의 배경색. 현재 선택된 값을 하이라이트. value_text가 이 위에 표시됨. bg와 구분되어 선택 영역을 표시",
    "value_bg": {},
    "__help_key__": "설정 다이얼로그 하단의 단축키(Enter, Esc 등). bg 위에 표시됨. 사용 가능한 키 안내",
    "help_key": {},
    "__help_text__": "설정 다이얼로그 하단의 도움말 텍스트. bg 위에 표시됨. help_key의 기능 설명",
    "help_text": {}
  }},

  "__editor__": "=== 파일 에디터: 내장 텍스트 편집기. 코드 편집, 설정 파일 수정 등에 사용. 전체 화면을 차지하는 뷰 ===",
  "editor": {{
    "__bg__": "에디터 편집 영역의 배경색. 코드/텍스트가 표시되는 메인 영역. text, syntax 하이라이팅, line_number, cursor 등 모든 편집 요소가 이 위에 렌더링됨. 장시간 편집에 적합한 눈의 피로가 적은 색상 권장",
    "bg": {},
    "__border__": "에디터 창의 테두리 색상. bg를 둘러싸며 에디터 영역을 정의. panel.bg와 에디터를 구분",
    "border": {},
    "__header_bg__": "에디터 상단 헤더의 배경색. 파일명과 커서 위치가 표시되는 영역. bg와 구분되어 헤더 영역임을 인식. header_text, header_info가 이 위에 표시됨. palette.bg_alt 참조 가능",
    "header_bg": {},
    "__header_text__": "헤더의 파일명 텍스트. header_bg 위에 표시됨. 현재 편집 중인 파일을 명확히 표시. header_info보다 높은 시각적 우선순위",
    "header_text": {},
    "__header_info__": "헤더의 커서 위치 정보(Line:Col). header_bg 위에 표시됨. header_text보다 낮은 시각적 강조. 현재 위치를 안내하는 보조 정보",
    "header_info": {},
    "__line_number__": "좌측 줄 번호 영역의 텍스트 색상. bg 위 좌측에 표시됨. text보다 낮은 시각적 강조로 본문과 구분. 편집 내용에 집중할 수 있도록 눈에 덜 띄게",
    "line_number": {},
    "__text__": "편집 영역의 기본 텍스트 색상. bg 위에 표시됨. 코드 하이라이팅이 없는 일반 텍스트. syntax 색상들과 함께 사용됨. bg와 높은 대비로 가독성 확보",
    "text": {},
    "__cursor__": "텍스트 커서(캐럿) 색상. bg 위에 표시됨. 현재 입력 위치를 명확히 표시. 깜빡이며 주의를 끔",
    "cursor": {},
    "__selection_bg__": "텍스트 선택 영역의 배경색. 드래그로 선택한 텍스트 블록의 하이라이트. bg와 확연히 구분되어야 함. selection_text가 이 위에 표시됨. panel.selected_bg와 유사한 역할",
    "selection_bg": {},
    "__selection_text__": "선택된 텍스트의 색상. selection_bg 위에 표시되므로 배경색과 대비되는 어두운 색이어야 함",
    "selection_text": {},
    "__match_bg__": "검색 결과 매치 부분의 배경색. Ctrl+F로 검색 시 일치하는 모든 부분 하이라이트. bg 위에 표시됨. match_current_bg와 구분되어 '다른 매치'임을 표시",
    "match_bg": {},
    "__match_current_bg__": "현재 포커스된 검색 결과의 배경색. 여러 매치 중 현재 위치한 매치를 강조. match_bg보다 더 눈에 띄어야 함. 현재 커서가 위치한 검색 결과",
    "match_current_bg": {},
    "__bracket_match__": "괄호 매칭 표시 색상. 커서가 괄호 위에 있을 때 대응하는 괄호 쌍을 하이라이트. bg 위에서 눈에 띄어야 함. 코드 구조 파악에 도움",
    "bracket_match": {},
    "__modified_mark__": "수정됨 표시(*) 색상. header 영역에서 파일이 수정되었음을 나타내는 표시. header_bg 위에 표시됨. 저장하지 않은 변경사항이 있음을 주의 환기",
    "modified_mark": {},
    "__footer_bg__": "에디터 하단바의 배경색. 단축키 안내와 검색/바꾸기 UI가 표시되는 영역. bg, header_bg와 구분되어 푸터 영역임을 인식. palette.bg_alt 참조 가능",
    "footer_bg": {},
    "__footer_key__": "하단바의 단축키 텍스트(Ctrl+S, Ctrl+F 등). footer_bg 위에 표시됨. footer_text와 쌍을 이루며 키 부분을 강조. palette.shortcut 참조",
    "footer_key": {},
    "__footer_text__": "하단바의 단축키 설명 텍스트(Save, Find 등). footer_bg 위에 표시됨. footer_key보다 낮은 시각적 강조",
    "footer_text": {},
    "__find_input_text__": "찾기/바꾸기 입력 필드의 텍스트. footer_bg 위에 표시됨. 사용자가 입력한 검색어가 명확히 보여야 함",
    "find_input_text": {},
    "__find_option__": "찾기 옵션(Case sensitive, Whole word)의 비활성 상태 색상. footer_bg 위에 표시됨. find_option_active보다 낮은 시각적 강조. 현재 꺼진 옵션",
    "find_option": {},
    "__find_option_active__": "찾기 옵션의 활성 상태 색상. footer_bg 위에 표시됨. find_option보다 높은 시각적 강조. 현재 켜진 옵션임을 명확히 표시",
    "find_option_active": {},
    "__wrap_indicator__": "줄 바꿈 표시자 색상. footer_bg 위에 표시됨. Word wrap 모드 활성화 시 하단바에 'Wrap' 텍스트로 표시",
    "wrap_indicator": {},
    "__remote_path_text__": "원격 파일 편집 시 헤더에 표시되는 [Remote: path] 텍스트 색상. header_bg 위에 표시됨",
    "remote_path_text": {}
  }},

  "__syntax__": "=== 코드 하이라이팅: 에디터/뷰어에서 프로그래밍 언어 문법을 색상으로 구분. 모두 editor.bg 또는 viewer.bg 위에 표시됨. 서로 구분되면서도 조화로운 색상 팔레트 필요 ===",
  "syntax": {{
    "__keyword__": "제어문/선언 키워드(if, else, for, while, fn, let, return, class, def 등). 언어의 구조를 정의하는 핵심 토큰. 다른 syntax 색상들과 구분되는 고유한 색상. 일반적으로 강조색 사용",
    "keyword": {},
    "__type_name__": "타입명(String, i32, Vec, int, bool, List 등). 데이터 타입을 나타내는 토큰. keyword와 구분되면서 의미적 연관성 가능. 일반적으로 keyword와 다른 색조",
    "type_name": {},
    "__string__": "문자열 리터럴(\"hello\", 'world', `template` 등). 따옴표로 둘러싸인 텍스트 데이터. 다른 코드와 명확히 구분되어야 함. 일반적으로 따뜻한 색상 계열",
    "string": {},
    "__number__": "숫자 리터럴(123, 3.14, 0xFF, 1e10 등). 수치 데이터. string과 함께 리터럴 값을 나타내지만 구분되어야 함",
    "number": {},
    "__comment__": "주석(// line comment, /* block */, # comment 등). 실행되지 않는 설명 텍스트. 다른 코드보다 낮은 시각적 강조. 읽을 수 있지만 코드에 집중할 수 있도록 눈에 덜 띄게",
    "comment": {},
    "__operator__": "연산자(+, -, *, /, =, ==, !=, &&, || 등). 연산을 수행하는 기호. normal과 같거나 약간 다른 색상. 지나치게 강조하지 않는 것이 일반적",
    "operator": {},
    "__function__": "함수/메서드 이름(main, print, calculate, onClick 등). 호출 가능한 코드 블록의 이름. keyword, variable과 구분되어 함수임을 인식",
    "function": {},
    "__macro_name__": "매크로(println!, vec!, #define, @decorator 등). 메타프로그래밍 요소. function과 구분되어 매크로/어노테이션임을 인식",
    "macro_name": {},
    "__attribute__": "어트리뷰트/데코레이터(#[derive], @Override, [Attribute] 등). 코드에 메타데이터를 추가하는 요소. comment와 유사하게 부가 정보이지만 의미 있는 요소",
    "attribute": {},
    "__variable__": "변수명. 데이터를 저장하는 식별자. normal과 같거나 유사할 수 있음. 다른 syntax 요소들과 대비되는 기본 텍스트 역할",
    "variable": {},
    "__constant__": "상수(CONST_VALUE, MAX_SIZE, true, false, null, None 등). 변경되지 않는 값. variable보다 강조되어 상수임을 인식. 대문자 상수와 예약어 포함",
    "constant": {},
    "__bracket__": "괄호류((), [], {{}}, <> 등). 코드 구조를 정의하는 구분자. operator와 유사하거나 같을 수 있음. editor.bracket_match와 연계",
    "bracket": {},
    "__normal__": "하이라이팅 규칙에 매칭되지 않는 일반 텍스트. editor.text와 동일하거나 유사. 기본 폴백 색상",
    "normal": {}
  }},

  "__viewer__": "=== 파일 뷰어: 읽기 전용 파일 보기. 텍스트/헥스 모드 지원. 전체 화면을 차지하는 뷰 ===",
  "viewer": {{
    "__bg__": "뷰어의 기본 배경색. 파일 내용이 표시되는 메인 영역. text, syntax 하이라이팅, line_number 등이 이 위에 렌더링됨. editor.bg와 동일하거나 유사 가능",
    "bg": {},
    "__border__": "뷰어 창의 테두리 색상. bg를 둘러싸며 뷰어 영역을 정의",
    "border": {},
    "__header_text__": "상단 헤더의 파일명 텍스트. 현재 보고 있는 파일을 표시. border 영역 또는 bg 상단에 표시됨",
    "header_text": {},
    "__line_number__": "좌측 줄 번호 색상. bg 위에 표시됨. text보다 낮은 시각적 강조. editor.line_number와 유사한 역할",
    "line_number": {},
    "__text__": "파일 내용의 기본 텍스트 색상. bg 위에 표시됨. 코드 파일의 경우 syntax 색상이 적용되지 않은 부분의 기본색. editor.text와 유사",
    "text": {},
    "__bookmark_indicator__": "북마크된 줄을 표시하는 인디케이터 색상. line_number 영역 또는 줄 배경에 표시됨. 사용자가 표시해둔 위치를 즉시 인식할 수 있어야 함",
    "bookmark_indicator": {},
    "__search_input_text__": "검색 입력 필드의 텍스트 색상. 하단 검색 UI에서 사용자가 입력한 검색어. bg 또는 별도 입력 영역 위에 표시됨",
    "search_input_text": {},
    "__search_cursor_fg__": "검색 입력 커서의 전경색. search_cursor_bg와 함께 현재 입력 위치 표시",
    "search_cursor_fg": {},
    "__search_cursor_bg__": "검색 입력 커서의 배경색. 현재 입력 위치를 하이라이트",
    "search_cursor_bg": {},
    "__search_match_current_bg__": "현재 포커스된 검색 매치의 배경색. 여러 매치 중 현재 위치한 매치를 강조. search_match_other_bg보다 더 눈에 띄어야 함. search_match_current_fg가 이 위에 표시됨",
    "search_match_current_bg": {},
    "__search_match_current_fg__": "현재 검색 매치의 텍스트 색상. search_match_current_bg 위에 표시됨. 매치된 텍스트가 읽을 수 있어야 함",
    "search_match_current_fg": {},
    "__search_match_other_bg__": "다른 검색 매치들의 배경색. 현재 포커스가 아닌 모든 매치 표시. search_match_current_bg보다 낮은 강조. search_match_other_fg가 이 위에 표시됨",
    "search_match_other_bg": {},
    "__search_match_other_fg__": "다른 검색 매치들의 텍스트 색상. search_match_other_bg 위에 표시됨",
    "search_match_other_fg": {},
    "__search_info__": "검색 정보 텍스트(1/10 matches, Not found 등). 검색 결과 수와 현재 위치를 안내. 보조 정보로서 주요 UI보다 낮은 강조",
    "search_info": {},
    "__hex_offset__": "헥스 뷰 모드에서 좌측 오프셋 주소(00000000:). bg 위에 표시됨. hex_bytes, hex_ascii와 시각적으로 구분되어 주소 영역임을 인식",
    "hex_offset": {},
    "__hex_bytes__": "헥스 뷰 모드에서 바이트 값(FF 0A 2B 등). bg 위에 표시됨. 실제 바이너리 데이터 표시. hex_offset, hex_ascii와 구분",
    "hex_bytes": {},
    "__hex_ascii__": "헥스 뷰 모드에서 ASCII 표현(우측). bg 위에 표시됨. 바이트의 문자 표현. hex_bytes와 쌍을 이루며 데이터 해석에 도움",
    "hex_ascii": {},
    "__wrap_indicator__": "줄 바꿈 표시자(↩ 또는 유사 기호). 긴 줄이 화면 너비를 초과하여 래핑될 때 표시. line_number 영역 또는 줄 끝에 표시. text보다 낮은 시각적 강조",
    "wrap_indicator": {},
    "__footer_key__": "하단 도움말의 단축키 텍스트. 사용 가능한 키 안내. editor.footer_key와 유사",
    "footer_key": {},
    "__footer_text__": "하단 도움말의 설명 텍스트. footer_key의 기능 설명",
    "footer_text": {}
  }},

  "__process_manager__": "=== 프로세스 관리자: 시스템에서 실행 중인 프로세스 목록과 리소스 사용량을 표시. 프로세스 종료 기능 제공 ===",
  "process_manager": {{
    "__bg__": "프로세스 관리자의 배경색. 프로세스 목록이 표시되는 메인 영역. text, column_header, selected_bg 등이 이 위에 표시됨",
    "bg": {},
    "__border__": "프로세스 관리자 창의 테두리 색상. bg를 둘러쌈",
    "border": {},
    "__header_text__": "상단 헤더의 텍스트. 창 제목 또는 시스템 정보 요약",
    "header_text": {},
    "__column_header__": "컬럼 헤더(PID, Name, CPU%, MEM% 등). bg 위에 표시됨. text와 구분되어 헤더행임을 인식. 정렬 가능한 컬럼 표시. palette.accent 참조",
    "column_header": {},
    "__text__": "프로세스 정보의 기본 텍스트. bg 위에 표시됨. 프로세스명, PID 등 일반 정보. cpu_high, mem_high와 대비되는 정상 상태 표시",
    "text": {},
    "__selected_bg__": "현재 선택된 프로세스 행의 배경색. bg와 확연히 구분되어 현재 포커스 표시. selected_text가 이 위에 표시됨. panel.selected_bg와 유사",
    "selected_bg": {},
    "__selected_text__": "선택된 프로세스의 텍스트 색상. selected_bg 위에 표시되므로 배경색과 대비되는 어두운 색이어야 함",
    "selected_text": {},
    "__cpu_high__": "높은 CPU 사용량 강조 색상. bg 위에 표시됨. 특정 임계값(예: 80%) 이상일 때 text 대신 사용. 주의가 필요한 프로세스를 즉시 인식. state.warning 또는 state.error와 유사",
    "cpu_high": {},
    "__mem_high__": "높은 메모리 사용량 강조 색상. cpu_high와 유사한 역할. 메모리 사용량이 높은 프로세스 강조",
    "mem_high": {},
    "__confirm_text__": "프로세스 종료 확인 메시지 텍스트. 위험한 작업 전 경고. bg 위에 표시되거나 별도 확인 UI에 표시",
    "confirm_text": {},
    "__footer_key__": "하단 도움말의 단축키(k:Kill, q:Quit 등). 프로세스 관리에 사용할 수 있는 키 안내",
    "footer_key": {},
    "__footer_text__": "하단 도움말의 설명 텍스트. footer_key의 기능 설명",
    "footer_text": {}
  }},

  "__ai_screen__": "=== AI 화면: Claude AI와 대화하는 채팅 인터페이스. 상단 히스토리 영역과 하단 입력 영역으로 구성 ===",
  "ai_screen": {{
    "__bg__": "AI 화면의 기본 배경색. 전체 채팅 인터페이스의 배경. 히스토리, 입력 영역 등이 이 위에 배치됨",
    "bg": {},
    "__history_border__": "대화 히스토리 영역의 테두리. bg 위에 히스토리 영역을 구분. input_border와 함께 화면 구조를 정의",
    "history_border": {},
    "__history_title__": "히스토리 영역 제목. 현재 세션 정보, 경로 등. history_border 내부 상단에 표시",
    "history_title": {},
    "__history_placeholder__": "대화가 없을 때 표시되는 플레이스홀더. bg 위에 표시됨. 'Start a conversation...' 등 안내 문구. message_text보다 낮은 시각적 강조",
    "history_placeholder": {},
    "__history_scroll_info__": "히스토리 스크롤 위치 정보. 긴 대화에서 현재 위치 안내",
    "history_scroll_info": {},
    "__user_prefix__": "사용자 메시지 접두사(> 또는 You:). bg 위에 표시됨. 사용자가 보낸 메시지를 구분. assistant_prefix와 대비되어 발신자 구별",
    "user_prefix": {},
    "__assistant_prefix__": "AI 응답 접두사(< 또는 AI:). bg 위에 표시됨. AI가 보낸 메시지를 구분. user_prefix와 다른 색상으로 대화 흐름 파악",
    "assistant_prefix": {},
    "__error_prefix__": "에러 메시지 접두사(!). 오류 발생 시 표시. 주의를 끄는 색상. state.error와 유사",
    "error_prefix": {},
    "__system_prefix__": "시스템 메시지 접두사(*). 연결 상태, 세션 정보 등 시스템 안내. user_prefix, assistant_prefix보다 낮은 강조",
    "system_prefix": {},
    "__message_text__": "메시지 본문 텍스트. bg 위에 표시됨. 대화 내용의 기본 색상. 가독성을 위해 bg와 높은 대비 필요",
    "message_text": {},
    "__input_border__": "입력 영역의 테두리. bg 위에 입력 영역을 구분. history_border와 함께 화면 구조 정의",
    "input_border": {},
    "__input_prompt__": "입력 프롬프트(>). 입력 영역에서 입력 시작점을 표시. user_prefix와 동일하거나 유사",
    "input_prompt": {},
    "__input_text__": "사용자 입력 텍스트. 입력 영역에서 타이핑 중인 메시지. message_text와 동일하거나 유사",
    "input_text": {},
    "__input_cursor_fg__": "입력 커서 전경색. 커서 위치의 문자 색상. input_cursor_bg 위에 표시됨",
    "input_cursor_fg": {},
    "__input_cursor_bg__": "입력 커서 배경색. 현재 타이핑 위치를 반전 블록으로 표시",
    "input_cursor_bg": {},
    "__input_placeholder__": "입력 플레이스홀더. 입력 전 안내 문구('Type a message...'). input_text보다 낮은 시각적 강조",
    "input_placeholder": {},
    "__processing_spinner__": "AI 응답 대기 중 스피너/로딩 표시 색상. 처리 중임을 시각적으로 안내",
    "processing_spinner": {},
    "__processing_text__": "처리 중 안내 텍스트('Thinking...'). processing_spinner와 함께 표시",
    "processing_text": {},
    "__error_text__": "에러 메시지 텍스트. API 오류, 연결 실패 등. state.error와 유사",
    "error_text": {},
    "__tool_use_prefix__": "도구 사용 표시 브래킷([]). AI가 도구를 사용할 때 표시. tool_use_name과 함께 도구 호출을 시각화",
    "tool_use_prefix": {},
    "__tool_use_name__": "도구 이름 텍스트(Bash, Write, Read 등). tool_use_prefix 옆에 표시. 어떤 도구가 사용되는지 명확히 구분",
    "tool_use_name": {},
    "__tool_use_input__": "도구 입력 내용 텍스트. 도구에 전달되는 명령이나 파라미터. message_text와 유사하거나 약간 흐리게",
    "tool_use_input": {},
    "__tool_result_prefix__": "도구 결과 표시 접두사(->). 도구 실행 결과 앞에 표시. tool_result_text와 함께 결과를 시각화",
    "tool_result_prefix": {},
    "__tool_result_text__": "도구 실행 결과 텍스트. 명령 출력, 파일 내용 등. message_text와 유사",
    "tool_result_text": {},
    "__footer_key__": "하단 도움말의 단축키. Enter:Send, Esc:Exit 등",
    "footer_key": {},
    "__footer_text__": "하단 도움말 설명",
    "footer_text": {}
  }},

  "__system_info__": "=== 시스템 정보: CPU, 메모리, 디스크 사용량 등 시스템 리소스 모니터링 화면. 탭으로 섹션 전환 ===",
  "system_info": {{
    "__bg__": "시스템 정보 화면의 배경색. 리소스 사용량 정보가 표시되는 메인 영역. section_title, label, value, bar 요소들이 이 위에 렌더링됨",
    "bg": {},
    "__border__": "시스템 정보 창의 테두리 색상. bg를 둘러쌈",
    "border": {},
    "__section_title__": "섹션 제목(CPU, Memory, Network 등). bg 위에 표시됨. 각 정보 그룹을 구분하는 헤더. label, value보다 높은 시각적 강조",
    "section_title": {},
    "__label__": "정보 레이블(Used:, Free:, Total: 등). bg 위에 표시됨. value와 쌍을 이루며 어떤 정보인지 안내",
    "label": {},
    "__value__": "정보 값(8GB, 45%, 192.168.1.1 등). bg 위에 표시됨. label과 쌍을 이룸. 실제 수치 표시",
    "value": {},
    "__bar_fill__": "사용량 바의 채워진 부분. 현재 사용량을 시각화. bar_empty와 함께 전체 대비 사용량 표현. palette.positive와 유사하거나 usage_low/medium/high에 따라 동적으로 변경 가능",
    "bar_fill": {},
    "__bar_empty__": "사용량 바의 빈 부분. 남은 용량/여유분을 나타냄. bar_fill과 함께 전체 용량 시각화",
    "bar_empty": {},
    "__usage_low__": "낮은 사용량 상태 색상(70% 미만). 정상적인 리소스 상태. state.success와 유사. 안심할 수 있는 색상",
    "usage_low": {},
    "__usage_medium__": "중간 사용량 상태 색상(70-90%). 주의가 필요한 수준. state.warning과 유사. 경고성 색상",
    "usage_medium": {},
    "__usage_high__": "높은 사용량 상태 색상(90% 이상). 위험한 리소스 상태. state.error와 유사. 즉각적인 주의 필요",
    "usage_high": {},
    "__tab_active__": "활성 탭의 표시 색상. 여러 정보 탭(Overview, CPU, Memory 등) 중 현재 선택된 탭. border 또는 별도 탭 영역에 표시",
    "tab_active": {},
    "__disk_header__": "디스크 섹션의 헤더. 마운트된 디스크 목록의 컬럼 헤더(Device, Mount, Size 등). section_title 또는 column_header와 유사",
    "disk_header": {},
    "__disk_text__": "디스크 정보 텍스트. 각 디스크의 상세 정보. value와 유사",
    "disk_text": {},
    "__selected_bg__": "선택된 항목의 배경색(디스크 목록에서). bg와 구분되어 현재 포커스 표시. selected_text가 이 위에 표시됨",
    "selected_bg": {},
    "__selected_text__": "선택된 항목의 텍스트 색상. selected_bg 위에 표시되므로 배경색과 대비되는 어두운 색이어야 함",
    "selected_text": {},
    "__footer_key__": "하단 도움말의 단축키",
    "footer_key": {},
    "__footer_text__": "하단 도움말 설명",
    "footer_text": {}
  }},

  "__search_result__": "=== 검색 결과: 파일/폴더 검색 결과를 목록으로 표시. 검색어 매치 하이라이트, 경로 탐색 기능 제공 ===",
  "search_result": {{
    "__bg__": "검색 결과 화면의 배경색. 검색된 파일/폴더 목록이 표시되는 영역. file_text, directory_text, selected_bg 등이 이 위에 렌더링됨",
    "bg": {},
    "__border__": "검색 결과 창의 테두리. bg를 둘러쌈",
    "border": {},
    "__header_text__": "상단 헤더 텍스트. 검색어, 결과 수 등 검색 정보 요약",
    "header_text": {},
    "__column_header__": "컬럼 헤더(Name, Path, Size, Date 등). bg 위에 표시됨. 결과 목록의 열 제목. palette.accent 참조",
    "column_header": {},
    "__column_header_dim__": "보조 컬럼 헤더. column_header보다 낮은 시각적 강조. 덜 중요한 컬럼에 사용",
    "column_header_dim": {},
    "__directory_text__": "검색 결과 중 디렉토리 항목. bg 위에 표시됨. file_text와 구분되어 폴더임을 인식. panel.directory_text와 유사",
    "directory_text": {},
    "__file_text__": "검색 결과 중 파일 항목. bg 위에 표시됨. 기본 결과 텍스트. panel.file_text와 유사",
    "file_text": {},
    "__selected_bg__": "선택된 검색 결과의 배경색. bg와 확연히 구분. selected_text가 이 위에 표시됨. 현재 포커스된 결과 항목",
    "selected_bg": {},
    "__selected_text__": "선택된 검색 결과의 텍스트 색상. selected_bg 위에 표시되므로 배경색과 대비되는 어두운 색이어야 함",
    "selected_text": {},
    "__match_highlight__": "검색어와 일치하는 부분의 하이라이트 색상. 파일명/경로에서 매치된 텍스트를 강조. bg 위에서 즉시 눈에 띄어야 함. palette.highlight 참조",
    "match_highlight": {},
    "__path_text__": "파일 경로 텍스트. bg 위에 표시됨. 검색된 파일의 위치를 표시. file_text보다 낮은 시각적 강조로 보조 정보 역할",
    "path_text": {},
    "__footer_key__": "하단 도움말의 단축키(Enter:Open, Esc:Close 등)",
    "footer_key": {},
    "__footer_text__": "하단 도움말 설명",
    "footer_text": {}
  }},

  "__image_viewer__": "=== 이미지 뷰어: 이미지 파일 미리보기. 줌, 패닝, 회전 기능 제공. 전체 화면 오버레이 ===",
  "image_viewer": {{
    "__bg__": "이미지 뷰어의 배경색. 이미지가 표시되는 영역의 배경. 이미지 외곽 여백 영역에 보임. 이미지 감상에 집중할 수 있는 중립적 색상 권장",
    "bg": {},
    "__border__": "이미지 뷰어 창의 테두리. bg를 둘러쌈",
    "border": {},
    "__title_text__": "제목바 텍스트. 파일명, 해상도(1920x1080), 줌 레벨(100%) 등 표시. border 상단 또는 bg 위에 오버레이",
    "title_text": {},
    "__loading_spinner__": "이미지 로딩 중 스피너/애니메이션 색상. bg 위 중앙에 표시. 로딩 상태를 시각적으로 안내",
    "loading_spinner": {},
    "__loading_text__": "로딩 중 안내 텍스트('Loading image...'). bg 위에 표시. loading_spinner와 함께 표시",
    "loading_text": {},
    "__error_text__": "이미지 로드 실패 시 에러 메시지. bg 위에 표시. 'Failed to load image' 등. state.error와 유사",
    "error_text": {},
    "__hint_text__": "조작 힌트 텍스트('Press ESC to close'). bg 위에 표시. 낮은 시각적 강조로 이미지 감상 방해하지 않게",
    "hint_text": {},
    "__footer_key__": "하단 도움말의 단축키(+/-:Zoom, Arrow:Pan 등)",
    "footer_key": {},
    "__footer_text__": "하단 도움말 설명",
    "footer_text": {},
    "__footer_separator__": "하단 도움말 항목 구분자(/). footer_key와 footer_text 사이 또는 항목 간 구분",
    "footer_separator": {}
  }},

  "__file_info__": "=== 파일 정보: 선택된 파일/폴더의 상세 정보를 표시하는 모달. 크기, 권한, 날짜 등 메타데이터 ===",
  "file_info": {{
    "__bg__": "파일 정보 다이얼로그의 배경색. 파일 메타데이터가 표시되는 모달 창. label, value 등 정보가 이 위에 표시됨",
    "bg": {},
    "__border__": "파일 정보 창의 테두리. bg를 둘러싸며 모달 영역 정의",
    "border": {},
    "__title__": "다이얼로그 제목('File Information'). bg 위 상단에 표시",
    "title": {},
    "__label__": "정보 레이블(Name:, Size:, Type:, Modified: 등). bg 위에 표시됨. value와 쌍을 이루며 어떤 정보인지 안내. value보다 낮은 시각적 강조",
    "label": {},
    "__value__": "정보 값의 기본 색상. bg 위에 표시됨. 특별히 지정되지 않은 값들의 기본색",
    "value": {},
    "__value_name__": "파일/폴더 이름 값. bg 위에 표시됨. 가장 중요한 정보로 다른 value보다 강조 가능. panel.directory_text 또는 palette.fg_strong 참조",
    "value_name": {},
    "__value_path__": "파일 경로 값. bg 위에 표시됨. 전체 경로 표시. value와 동일하거나 유사",
    "value_path": {},
    "__value_type__": "파일 타입 값(Directory, Text File, Image 등). bg 위에 표시됨",
    "value_type": {},
    "__value_size__": "파일 크기 값(1.5 MB, 계산 중... 등). bg 위에 표시됨. 숫자 정보로 강조 가능",
    "value_size": {},
    "__value_permission__": "파일 권한 값(rwxr-xr-x, 755 등). bg 위에 표시됨",
    "value_permission": {},
    "__value_owner__": "소유자/그룹 값(user:group). bg 위에 표시됨",
    "value_owner": {},
    "__value_date__": "날짜/시간 값(수정일, 생성일 등). bg 위에 표시됨",
    "value_date": {},
    "__calculating_spinner__": "폴더 크기 계산 중 스피너. 디렉토리 크기 계산 시 표시. bg 위에 value_size 옆 또는 대신 표시",
    "calculating_spinner": {},
    "__calculating_text__": "계산 중 텍스트('Calculating...'). calculating_spinner와 함께 표시",
    "calculating_text": {},
    "__error_text__": "정보 읽기 실패 시 에러 메시지. bg 위에 표시. state.error와 유사",
    "error_text": {},
    "__hint_text__": "하단 힌트 텍스트('Press any key to close'). bg 위에 표시",
    "hint_text": {}
  }},

  "__help__": "=== 도움말 화면: 사용 가능한 모든 단축키를 섹션별로 정리하여 표시. 키보드 기반 파일 관리자의 핵심 참조 자료 ===",
  "help": {{
    "__bg__": "도움말 화면의 배경색. 단축키 목록이 표시되는 모달 창. section_title, key, description 등이 이 위에 렌더링됨",
    "bg": {},
    "__border__": "도움말 창의 테두리. bg를 둘러쌈",
    "border": {},
    "__title__": "도움말 창 제목('Keyboard Shortcuts' 등). bg 위 상단에 표시",
    "title": {},
    "__section_title__": "단축키 그룹 제목(Navigation, File Operations, View, Search 등). bg 위에 표시됨. 단축키들을 기능별로 분류. key, description보다 높은 시각적 강조",
    "section_title": {},
    "__section_decorator__": "섹션 제목 장식선(──, === 등). bg 위에 표시됨. 섹션 간 구분을 시각화. section_title보다 낮은 강조",
    "section_decorator": {},
    "__key__": "단축키 텍스트(Ctrl+C, Space 등). bg 위에 표시됨. description과 쌍을 이룸. 키 부분을 강조하여 빠른 참조 가능. palette.shortcut 참조",
    "key": {},
    "__key_highlight__": "특별히 강조되는 단축키. key 중에서 더 중요하거나 자주 사용되는 키. key와 다른 색상으로 추가 강조",
    "key_highlight": {},
    "__description__": "단축키 설명 텍스트('Open file', 'Copy to clipboard' 등). bg 위에 표시됨. key 옆에 위치하여 해당 키의 기능 설명. key보다 낮은 시각적 강조",
    "description": {},
    "__hint_text__": "하단 힌트 텍스트('Press any key to close'). bg 위에 표시됨. 창 닫는 방법 안내",
    "hint_text": {}
  }},

  "__advanced_search__": "=== 고급 검색: 파일명 패턴, 크기 범위, 날짜 필터 등 상세 검색 조건을 설정하는 폼 다이얼로그 ===",
  "advanced_search": {{
    "__bg__": "고급 검색 다이얼로그의 배경색. 검색 조건 입력 폼이 표시되는 모달 창. label, input, checkbox, button 등이 이 위에 렌더링됨",
    "bg": {},
    "__border__": "고급 검색 창의 테두리. bg를 둘러쌈",
    "border": {},
    "__title__": "다이얼로그 제목('Advanced Search'). bg 위 상단에 표시",
    "title": {},
    "__label__": "검색 조건 레이블(Pattern:, Size:, Date: 등). bg 위에 표시됨. 각 입력 필드가 무엇인지 안내. input_text와 쌍을 이룸",
    "label": {},
    "__input_text__": "검색 조건 입력 텍스트. bg 위에 표시됨. 사용자가 입력한 검색 패턴, 크기 조건 등",
    "input_text": {},
    "__input_cursor__": "입력 필드 커서. 현재 입력 위치 표시",
    "input_cursor": {},
    "__field_bracket__": "입력 필드 괄호([, ]). bg 위에 표시됨. 입력 영역의 시작과 끝을 표시. label과 구분되는 색상",
    "field_bracket": {},
    "__checkbox_checked__": "체크된 체크박스 색상([x] 또는 [✓]). bg 위에 표시됨. 옵션이 활성화됨을 표시. palette.positive 참조. checkbox_unchecked와 명확히 구분",
    "checkbox_checked": {},
    "__checkbox_unchecked__": "체크 안 된 체크박스 색상([ ]). bg 위에 표시됨. 옵션이 비활성화됨을 표시. checkbox_checked보다 낮은 시각적 강조",
    "checkbox_unchecked": {},
    "__button_text__": "비선택 버튼의 텍스트(Search, Cancel 등). bg 위에 표시됨. button_selected_text보다 낮은 강조",
    "button_text": {},
    "__button_selected_bg__": "선택된 버튼의 배경색. 현재 포커스가 있는 버튼 하이라이트. button_selected_text가 이 위에 표시됨",
    "button_selected_bg": {},
    "__button_selected_text__": "선택된 버튼의 텍스트 색상. button_selected_bg 위에 표시됨",
    "button_selected_text": {},
    "__footer_key__": "하단 도움말의 단축키(Tab:Next field, Enter:Search 등)",
    "footer_key": {},
    "__footer_text__": "하단 도움말 설명",
    "footer_text": {}
  }},

  "__diff__": "=== Diff 화면: 두 폴더를 재귀적으로 비교하여 차이점을 시각적으로 표시하는 화면 ===",
  "diff": {{
    "__bg__": "DIFF 화면 배경색",
    "bg": {},
    "__border__": "DIFF 패널 테두리",
    "border": {},
    "__header_text__": "DIFF 헤더 경로 텍스트",
    "header_text": {},
    "__header_label__": "[DIFF] 라벨 텍스트",
    "header_label": {},
    "__column_header_bg__": "컬럼 헤더 배경",
    "column_header_bg": {},
    "__column_header_text__": "컬럼 헤더 텍스트",
    "column_header_text": {},
    "__same_text__": "동일 파일 텍스트",
    "same_text": {},
    "__modified_text__": "변경된 파일 텍스트",
    "modified_text": {},
    "__modified_bg__": "변경된 파일 배경",
    "modified_bg": {},
    "__left_only_text__": "한쪽에만 존재하는 파일 텍스트 (왼쪽) - right_only_text와 동일한 값을 사용할 것",
    "left_only_text": {},
    "__left_only_bg__": "한쪽에만 존재하는 파일 배경 (왼쪽) - right_only_bg와 동일한 값을 사용할 것",
    "left_only_bg": {},
    "__right_only_text__": "한쪽에만 존재하는 파일 텍스트 (오른쪽) - left_only_text와 동일한 값을 사용할 것",
    "right_only_text": {},
    "__right_only_bg__": "한쪽에만 존재하는 파일 배경 (오른쪽) - left_only_bg와 동일한 값을 사용할 것",
    "right_only_bg": {},
    "__empty_bg__": "반대쪽에 없는 항목의 빈칸 배경",
    "empty_bg": {},
    "__dir_same_text__": "동일 디렉토리 텍스트",
    "dir_same_text": {},
    "__dir_modified_text__": "하위 내용이 다른 디렉토리 텍스트",
    "dir_modified_text": {},
    "__cursor_bg__": "커서 bar 배경",
    "cursor_bg": {},
    "__cursor_text__": "커서 bar 텍스트",
    "cursor_text": {},
    "__marked_text__": "Space로 마킹된 항목 텍스트",
    "marked_text": {},
    "__size_text__": "파일 크기 텍스트",
    "size_text": {},
    "__date_text__": "수정 날짜 텍스트",
    "date_text": {},
    "__status_bar_bg__": "상태 바 배경",
    "status_bar_bg": {},
    "__status_bar_text__": "상태 바 텍스트",
    "status_bar_text": {},
    "__filter_label__": "필터 라벨 텍스트",
    "filter_label": {},
    "__stats_text__": "통계 수치 텍스트",
    "stats_text": {},
    "__footer_key__": "기능 바 단축키",
    "footer_key": {},
    "__footer_text__": "기능 바 설명",
    "footer_text": {},
    "__panel_selected_border__": "3개 이상 패널에서 DIFF 첫 번째 선택 시 테두리",
    "panel_selected_border": {},
    "__progress_spinner__": "비교 진행 중 스피너 색상",
    "progress_spinner": {},
    "__progress_bar_fill__": "비교 프로그레스 바 채움",
    "progress_bar_fill": {},
    "__progress_bar_empty__": "비교 프로그레스 바 빈 부분",
    "progress_bar_empty": {},
    "__progress_percent_text__": "비교 프로그레스 퍼센트 텍스트",
    "progress_percent_text": {},
    "__progress_value_text__": "비교 중 현재 파일 경로 텍스트",
    "progress_value_text": {},
    "__progress_hint_text__": "비교 중 ESC 힌트 텍스트",
    "progress_hint_text": {}
  }},

  "__diff_file_view__": "=== Diff File View: 파일 내용을 좌우 나란히 라인별로 비교하는 화면 ===",
  "diff_file_view": {{
    "__bg__": "배경색",
    "bg": {},
    "__border__": "테두리",
    "border": {},
    "__header_text__": "헤더 텍스트",
    "header_text": {},
    "__line_number__": "라인 번호",
    "line_number": {},
    "__same_text__": "동일 라인 텍스트",
    "same_text": {},
    "__modified_text__": "변경된 라인 텍스트",
    "modified_text": {},
    "__modified_bg__": "변경된 라인 배경",
    "modified_bg": {},
    "__left_only_text__": "한쪽에만 있는 라인 텍스트 (왼쪽) - right_only_text와 동일한 값을 사용할 것",
    "left_only_text": {},
    "__left_only_bg__": "한쪽에만 있는 라인 배경 (왼쪽) - right_only_bg와 동일한 값을 사용할 것",
    "left_only_bg": {},
    "__right_only_text__": "한쪽에만 있는 라인 텍스트 (오른쪽) - left_only_text와 동일한 값을 사용할 것",
    "right_only_text": {},
    "__right_only_bg__": "한쪽에만 있는 라인 배경 (오른쪽) - left_only_bg와 동일한 값을 사용할 것",
    "right_only_bg": {},
    "__empty_bg__": "빈 라인 배경",
    "empty_bg": {},
    "__inline_change_bg__": "인라인 변경 하이라이트 배경",
    "inline_change_bg": {},
    "__inline_change_text__": "인라인 변경 하이라이트 텍스트",
    "inline_change_text": {},
    "__status_bar_bg__": "상태 바 배경",
    "status_bar_bg": {},
    "__status_bar_text__": "상태 바 텍스트",
    "status_bar_text": {},
    "__footer_key__": "기능 바 단축키",
    "footer_key": {},
    "__footer_text__": "기능 바 설명",
    "footer_text": {}
  }},

  "__git_screen__": "=== Git Screen: Git 저장소 상태, 커밋, 로그, 브랜치를 관리하는 전용 화면 ===",
  "git_screen": {{
    "__bg__": "배경색",
    "bg": {},
    "__border__": "테두리",
    "border": {},
    "__header_branch__": "헤더 브랜치명 텍스트",
    "header_branch": {},
    "__header_path__": "헤더 경로 텍스트",
    "header_path": {},
    "__tab_active__": "활성 탭 텍스트",
    "tab_active": {},
    "__tab_inactive__": "비활성 탭 텍스트",
    "tab_inactive": {},
    "__tab_bar_bg__": "탭 바 배경",
    "tab_bar_bg": {},
    "__file_staged__": "Staged 파일 텍스트",
    "file_staged": {},
    "__file_modified__": "Modified 파일 텍스트",
    "file_modified": {},
    "__file_untracked__": "Untracked 파일 텍스트",
    "file_untracked": {},
    "__file_deleted__": "Deleted 파일 텍스트",
    "file_deleted": {},
    "__selected_bg__": "선택된 항목 배경",
    "selected_bg": {},
    "__selected_text__": "선택된 항목 텍스트",
    "selected_text": {},
    "__footer_key__": "기능 바 단축키",
    "footer_key": {},
    "__footer_text__": "기능 바 설명",
    "footer_text": {},
    "__commit_input_border__": "커밋 메시지 입력란 테두리",
    "commit_input_border": {},
    "__commit_input_text__": "커밋 메시지 입력란 텍스트",
    "commit_input_text": {},
    "__log_hash__": "로그 커밋 해시",
    "log_hash": {},
    "__log_message__": "로그 커밋 메시지",
    "log_message": {},
    "__log_author__": "로그 커밋 작성자",
    "log_author": {},
    "__log_date__": "로그 커밋 날짜",
    "log_date": {},
    "__branch_current__": "현재 브랜치 텍스트",
    "branch_current": {},
    "__branch_normal__": "일반 브랜치 텍스트",
    "branch_normal": {},
    "__diff_add__": "Diff 추가 라인",
    "diff_add": {},
    "__diff_remove__": "Diff 삭제 라인",
    "diff_remove": {},
    "__diff_header__": "Diff 헤더",
    "diff_header": {}
  }},

  "__dedup_screen__": "=== 중복 파일 제거 화면: Shift+X로 진입하는 전체화면 중복 제거 UI ===",
  "dedup_screen": {{
    "__bg__": "배경색",
    "bg": {},
    "__border__": "테두리",
    "border": {},
    "__title__": "제목 텍스트",
    "title": {},
    "__phase_text__": "현재 단계 텍스트 (Scanning/Hashing/Deleting/Complete)",
    "phase_text": {},
    "__stats_text__": "통계 레이블 텍스트",
    "stats_text": {},
    "__progress_bar_fill__": "진행률 바 채움",
    "progress_bar_fill": {},
    "__progress_bar_empty__": "진행률 바 빈 영역",
    "progress_bar_empty": {},
    "__progress_text__": "진행률 텍스트 (현재 파일 경로)",
    "progress_text": {},
    "__log_text__": "로그 텍스트 (hash, size 등 주요 정보)",
    "log_text": {},
    "__log_text_alt__": "로그 텍스트 (진행률, 경로 등 보조 정보)",
    "log_text_alt": {},
    "__log_deleted__": "삭제된 파일 로그 텍스트",
    "log_deleted": {},
    "__log_error__": "에러 로그 텍스트",
    "log_error": {},
    "__footer_key__": "기능 바 단축키",
    "footer_key": {},
    "__footer_text__": "기능 바 설명",
    "footer_text": {}
  }}
}}"#,
            // name
            self.name(),
            // palette
            ci(self.palette.bg),
            ci(self.palette.bg_alt),
            ci(self.palette.fg),
            ci(self.palette.fg_dim),
            ci(self.palette.fg_strong),
            ci(self.palette.fg_inverse),
            ci(self.palette.accent),
            ci(self.palette.shortcut),
            ci(self.palette.positive),
            ci(self.palette.highlight),
            // state
            ci(self.state.success),
            ci(self.state.warning),
            ci(self.state.error),
            ci(self.state.info),
            // panel
            ci(self.panel.bg),
            ci(self.panel.border),
            ci(self.panel.border_active),
            ci(self.panel.header_bg),
            ci(self.panel.header_bg_active),
            ci(self.panel.header_text),
            ci(self.panel.header_text_active),
            ci(self.panel.file_text),
            ci(self.panel.directory_text),
            ci(self.panel.symlink_text),
            ci(self.panel.selected_bg),
            ci(self.panel.selected_text),
            ci(self.panel.marked_text),
            ci(self.panel.size_text),
            ci(self.panel.date_text),
            ci(self.panel.remote_indicator),
            // header
            ci(self.header.bg),
            ci(self.header.text),
            ci(self.header.title),
            // status_bar
            ci(self.status_bar.bg),
            ci(self.status_bar.text),
            ci(self.status_bar.text_dim),
            // function_bar
            ci(self.function_bar.bg),
            ci(self.function_bar.key),
            ci(self.function_bar.label),
            // message
            ci(self.message.bg),
            ci(self.message.text),
            // dialog
            ci(self.dialog.bg),
            ci(self.dialog.border),
            ci(self.dialog.title),
            ci(self.dialog.text),
            ci(self.dialog.text_dim),
            ci(self.dialog.message_text),
            ci(self.dialog.input_text),
            ci(self.dialog.input_cursor_fg),
            ci(self.dialog.input_cursor_bg),
            ci(self.dialog.input_prompt),
            ci(self.dialog.button_text),
            ci(self.dialog.button_selected_bg),
            ci(self.dialog.button_selected_text),
            ci(self.dialog.autocomplete_bg),
            ci(self.dialog.autocomplete_text),
            ci(self.dialog.autocomplete_directory_text),
            ci(self.dialog.autocomplete_selected_bg),
            ci(self.dialog.autocomplete_selected_text),
            ci(self.dialog.autocomplete_scroll_info),
            ci(self.dialog.preview_suffix_text),
            ci(self.dialog.help_key_text),
            ci(self.dialog.help_label_text),
            ci(self.dialog.progress_label_text),
            ci(self.dialog.progress_value_text),
            ci(self.dialog.progress_bar_fill),
            ci(self.dialog.progress_bar_empty),
            ci(self.dialog.progress_percent_text),
            ci(self.dialog.conflict_filename_text),
            ci(self.dialog.conflict_count_text),
            ci(self.dialog.conflict_shortcut_text),
            ci(self.dialog.tar_exclude_title),
            ci(self.dialog.tar_exclude_border),
            ci(self.dialog.tar_exclude_bg),
            ci(self.dialog.tar_exclude_message_text),
            ci(self.dialog.tar_exclude_path_text),
            ci(self.dialog.tar_exclude_scroll_info),
            ci(self.dialog.tar_exclude_button_text),
            ci(self.dialog.tar_exclude_button_selected_bg),
            ci(self.dialog.tar_exclude_button_selected_text),
            ci(self.dialog.git_log_diff_title),
            ci(self.dialog.git_log_diff_border),
            ci(self.dialog.git_log_diff_bg),
            ci(self.dialog.git_log_diff_message_text),
            ci(self.dialog.git_log_diff_entry_text),
            ci(self.dialog.git_log_diff_selected_text),
            ci(self.dialog.git_log_diff_cursor_text),
            ci(self.dialog.git_log_diff_cursor_bg),
            ci(self.dialog.git_log_diff_button_text),
            ci(self.dialog.git_log_diff_button_selected_text),
            ci(self.dialog.git_log_diff_button_selected_bg),
            ci(self.dialog.git_log_diff_button_disabled_text),
            ci(self.dialog.git_log_diff_scroll_info),
            ci(self.dialog.remote_bookmark_text),
            ci(self.dialog.remote_connect_field_label),
            ci(self.dialog.remote_connect_field_value),
            ci(self.dialog.remote_connect_field_selected_bg),
            // confirm_dialog
            ci(self.confirm_dialog.bg),
            ci(self.confirm_dialog.border),
            ci(self.confirm_dialog.title),
            ci(self.confirm_dialog.message_text),
            ci(self.confirm_dialog.button_text),
            ci(self.confirm_dialog.button_selected_bg),
            ci(self.confirm_dialog.button_selected_text),
            // settings
            ci(self.settings.bg),
            ci(self.settings.border),
            ci(self.settings.title),
            ci(self.settings.label_text),
            ci(self.settings.prompt),
            ci(self.settings.value_text),
            ci(self.settings.value_bg),
            ci(self.settings.help_key),
            ci(self.settings.help_text),
            // editor
            ci(self.editor.bg),
            ci(self.editor.border),
            ci(self.editor.header_bg),
            ci(self.editor.header_text),
            ci(self.editor.header_info),
            ci(self.editor.line_number),
            ci(self.editor.text),
            ci(self.editor.cursor),
            ci(self.editor.selection_bg),
            ci(self.editor.selection_text),
            ci(self.editor.match_bg),
            ci(self.editor.match_current_bg),
            ci(self.editor.bracket_match),
            ci(self.editor.modified_mark),
            ci(self.editor.footer_bg),
            ci(self.editor.footer_key),
            ci(self.editor.footer_text),
            ci(self.editor.find_input_text),
            ci(self.editor.find_option),
            ci(self.editor.find_option_active),
            ci(self.editor.wrap_indicator),
            ci(self.editor.remote_path_text),
            // syntax
            ci(self.syntax.keyword),
            ci(self.syntax.type_name),
            ci(self.syntax.string),
            ci(self.syntax.number),
            ci(self.syntax.comment),
            ci(self.syntax.operator),
            ci(self.syntax.function),
            ci(self.syntax.macro_name),
            ci(self.syntax.attribute),
            ci(self.syntax.variable),
            ci(self.syntax.constant),
            ci(self.syntax.bracket),
            ci(self.syntax.normal),
            // viewer
            ci(self.viewer.bg),
            ci(self.viewer.border),
            ci(self.viewer.header_text),
            ci(self.viewer.line_number),
            ci(self.viewer.text),
            ci(self.viewer.bookmark_indicator),
            ci(self.viewer.search_input_text),
            ci(self.viewer.search_cursor_fg),
            ci(self.viewer.search_cursor_bg),
            ci(self.viewer.search_match_current_bg),
            ci(self.viewer.search_match_current_fg),
            ci(self.viewer.search_match_other_bg),
            ci(self.viewer.search_match_other_fg),
            ci(self.viewer.search_info),
            ci(self.viewer.hex_offset),
            ci(self.viewer.hex_bytes),
            ci(self.viewer.hex_ascii),
            ci(self.viewer.wrap_indicator),
            ci(self.viewer.footer_key),
            ci(self.viewer.footer_text),
            // process_manager
            ci(self.process_manager.bg),
            ci(self.process_manager.border),
            ci(self.process_manager.header_text),
            ci(self.process_manager.column_header),
            ci(self.process_manager.text),
            ci(self.process_manager.selected_bg),
            ci(self.process_manager.selected_text),
            ci(self.process_manager.cpu_high),
            ci(self.process_manager.mem_high),
            ci(self.process_manager.confirm_text),
            ci(self.process_manager.footer_key),
            ci(self.process_manager.footer_text),
            // ai_screen
            ci(self.ai_screen.bg),
            ci(self.ai_screen.history_border),
            ci(self.ai_screen.history_title),
            ci(self.ai_screen.history_placeholder),
            ci(self.ai_screen.history_scroll_info),
            ci(self.ai_screen.user_prefix),
            ci(self.ai_screen.assistant_prefix),
            ci(self.ai_screen.error_prefix),
            ci(self.ai_screen.system_prefix),
            ci(self.ai_screen.message_text),
            ci(self.ai_screen.input_border),
            ci(self.ai_screen.input_prompt),
            ci(self.ai_screen.input_text),
            ci(self.ai_screen.input_cursor_fg),
            ci(self.ai_screen.input_cursor_bg),
            ci(self.ai_screen.input_placeholder),
            ci(self.ai_screen.processing_spinner),
            ci(self.ai_screen.processing_text),
            ci(self.ai_screen.error_text),
            ci(self.ai_screen.tool_use_prefix),
            ci(self.ai_screen.tool_use_name),
            ci(self.ai_screen.tool_use_input),
            ci(self.ai_screen.tool_result_prefix),
            ci(self.ai_screen.tool_result_text),
            ci(self.ai_screen.footer_key),
            ci(self.ai_screen.footer_text),
            // system_info
            ci(self.system_info.bg),
            ci(self.system_info.border),
            ci(self.system_info.section_title),
            ci(self.system_info.label),
            ci(self.system_info.value),
            ci(self.system_info.bar_fill),
            ci(self.system_info.bar_empty),
            ci(self.system_info.usage_low),
            ci(self.system_info.usage_medium),
            ci(self.system_info.usage_high),
            ci(self.system_info.tab_active),
            ci(self.system_info.disk_header),
            ci(self.system_info.disk_text),
            ci(self.system_info.selected_bg),
            ci(self.system_info.selected_text),
            ci(self.system_info.footer_key),
            ci(self.system_info.footer_text),
            // search_result
            ci(self.search_result.bg),
            ci(self.search_result.border),
            ci(self.search_result.header_text),
            ci(self.search_result.column_header),
            ci(self.search_result.column_header_dim),
            ci(self.search_result.directory_text),
            ci(self.search_result.file_text),
            ci(self.search_result.selected_bg),
            ci(self.search_result.selected_text),
            ci(self.search_result.match_highlight),
            ci(self.search_result.path_text),
            ci(self.search_result.footer_key),
            ci(self.search_result.footer_text),
            // image_viewer
            ci(self.image_viewer.bg),
            ci(self.image_viewer.border),
            ci(self.image_viewer.title_text),
            ci(self.image_viewer.loading_spinner),
            ci(self.image_viewer.loading_text),
            ci(self.image_viewer.error_text),
            ci(self.image_viewer.hint_text),
            ci(self.image_viewer.footer_key),
            ci(self.image_viewer.footer_text),
            ci(self.image_viewer.footer_separator),
            // file_info
            ci(self.file_info.bg),
            ci(self.file_info.border),
            ci(self.file_info.title),
            ci(self.file_info.label),
            ci(self.file_info.value),
            ci(self.file_info.value_name),
            ci(self.file_info.value_path),
            ci(self.file_info.value_type),
            ci(self.file_info.value_size),
            ci(self.file_info.value_permission),
            ci(self.file_info.value_owner),
            ci(self.file_info.value_date),
            ci(self.file_info.calculating_spinner),
            ci(self.file_info.calculating_text),
            ci(self.file_info.error_text),
            ci(self.file_info.hint_text),
            // help
            ci(self.help.bg),
            ci(self.help.border),
            ci(self.help.title),
            ci(self.help.section_title),
            ci(self.help.section_decorator),
            ci(self.help.key),
            ci(self.help.key_highlight),
            ci(self.help.description),
            ci(self.help.hint_text),
            // advanced_search
            ci(self.advanced_search.bg),
            ci(self.advanced_search.border),
            ci(self.advanced_search.title),
            ci(self.advanced_search.label),
            ci(self.advanced_search.input_text),
            ci(self.advanced_search.input_cursor),
            ci(self.advanced_search.field_bracket),
            ci(self.advanced_search.checkbox_checked),
            ci(self.advanced_search.checkbox_unchecked),
            ci(self.advanced_search.button_text),
            ci(self.advanced_search.button_selected_bg),
            ci(self.advanced_search.button_selected_text),
            ci(self.advanced_search.footer_key),
            ci(self.advanced_search.footer_text),
            // diff
            ci(self.diff.bg),
            ci(self.diff.border),
            ci(self.diff.header_text),
            ci(self.diff.header_label),
            ci(self.diff.column_header_bg),
            ci(self.diff.column_header_text),
            ci(self.diff.same_text),
            ci(self.diff.modified_text),
            ci(self.diff.modified_bg),
            ci(self.diff.left_only_text),
            ci(self.diff.left_only_bg),
            ci(self.diff.right_only_text),
            ci(self.diff.right_only_bg),
            ci(self.diff.empty_bg),
            ci(self.diff.dir_same_text),
            ci(self.diff.dir_modified_text),
            ci(self.diff.cursor_bg),
            ci(self.diff.cursor_text),
            ci(self.diff.marked_text),
            ci(self.diff.size_text),
            ci(self.diff.date_text),
            ci(self.diff.status_bar_bg),
            ci(self.diff.status_bar_text),
            ci(self.diff.filter_label),
            ci(self.diff.stats_text),
            ci(self.diff.footer_key),
            ci(self.diff.footer_text),
            ci(self.diff.panel_selected_border),
            ci(self.diff.progress_spinner),
            ci(self.diff.progress_bar_fill),
            ci(self.diff.progress_bar_empty),
            ci(self.diff.progress_percent_text),
            ci(self.diff.progress_value_text),
            ci(self.diff.progress_hint_text),
            // diff_file_view
            ci(self.diff_file_view.bg),
            ci(self.diff_file_view.border),
            ci(self.diff_file_view.header_text),
            ci(self.diff_file_view.line_number),
            ci(self.diff_file_view.same_text),
            ci(self.diff_file_view.modified_text),
            ci(self.diff_file_view.modified_bg),
            ci(self.diff_file_view.left_only_text),
            ci(self.diff_file_view.left_only_bg),
            ci(self.diff_file_view.right_only_text),
            ci(self.diff_file_view.right_only_bg),
            ci(self.diff_file_view.empty_bg),
            ci(self.diff_file_view.inline_change_bg),
            ci(self.diff_file_view.inline_change_text),
            ci(self.diff_file_view.status_bar_bg),
            ci(self.diff_file_view.status_bar_text),
            ci(self.diff_file_view.footer_key),
            ci(self.diff_file_view.footer_text),
            // git_screen
            ci(self.git_screen.bg),
            ci(self.git_screen.border),
            ci(self.git_screen.header_branch),
            ci(self.git_screen.header_path),
            ci(self.git_screen.tab_active),
            ci(self.git_screen.tab_inactive),
            ci(self.git_screen.tab_bar_bg),
            ci(self.git_screen.file_staged),
            ci(self.git_screen.file_modified),
            ci(self.git_screen.file_untracked),
            ci(self.git_screen.file_deleted),
            ci(self.git_screen.selected_bg),
            ci(self.git_screen.selected_text),
            ci(self.git_screen.footer_key),
            ci(self.git_screen.footer_text),
            ci(self.git_screen.commit_input_border),
            ci(self.git_screen.commit_input_text),
            ci(self.git_screen.log_hash),
            ci(self.git_screen.log_message),
            ci(self.git_screen.log_author),
            ci(self.git_screen.log_date),
            ci(self.git_screen.branch_current),
            ci(self.git_screen.branch_normal),
            ci(self.git_screen.diff_add),
            ci(self.git_screen.diff_remove),
            ci(self.git_screen.diff_header),
            // dedup_screen
            ci(self.dedup_screen.bg),
            ci(self.dedup_screen.border),
            ci(self.dedup_screen.title),
            ci(self.dedup_screen.phase_text),
            ci(self.dedup_screen.stats_text),
            ci(self.dedup_screen.progress_bar_fill),
            ci(self.dedup_screen.progress_bar_empty),
            ci(self.dedup_screen.progress_text),
            ci(self.dedup_screen.log_text),
            ci(self.dedup_screen.log_text_alt),
            ci(self.dedup_screen.log_deleted),
            ci(self.dedup_screen.log_error),
            ci(self.dedup_screen.footer_key),
            ci(self.dedup_screen.footer_text),
        )
    }
}
