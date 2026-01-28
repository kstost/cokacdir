use std::fs;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use super::{
    app::{App, Dialog, DialogType, PathCompletion},
    theme::Theme,
};

/// 경로 문자열을 확장 (~ 홈 경로 확장)
fn expand_path_string(input: &str) -> PathBuf {
    if input.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            let rest = input.strip_prefix('~').unwrap_or("");
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            if rest.is_empty() {
                home
            } else {
                home.join(rest)
            }
        } else {
            PathBuf::from(input)
        }
    } else {
        PathBuf::from(input)
    }
}

/// 입력 경로를 (기준 디렉토리, 접두어)로 분리
/// `~` 홈 경로 확장 처리
fn parse_path_for_completion(input: &str) -> (PathBuf, String) {
    // `~` 확장
    let expanded = if input.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            let rest = input.strip_prefix('~').unwrap_or("");
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            if rest.is_empty() {
                home.display().to_string()
            } else {
                home.join(rest).display().to_string()
            }
        } else {
            input.to_string()
        }
    } else {
        input.to_string()
    };

    let path = PathBuf::from(&expanded);

    // 입력이 /로 끝나면 해당 디렉토리 내부 검색
    if expanded.ends_with('/') || expanded.ends_with(std::path::MAIN_SEPARATOR) {
        return (path, String::new());
    }

    // 파일명 부분과 디렉토리 부분 분리
    if let Some(parent) = path.parent() {
        let prefix = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        (parent.to_path_buf(), prefix)
    } else {
        // 루트 경로인 경우
        (PathBuf::from("/"), String::new())
    }
}

/// 디렉토리 읽기 및 접두어 매칭
/// 대소문자 무시 검색, 디렉토리 우선 정렬
fn get_path_suggestions(base_dir: &PathBuf, prefix: &str) -> Vec<String> {
    let mut suggestions: Vec<(String, bool)> = Vec::new();
    let lower_prefix = prefix.to_lowercase();

    if let Ok(entries) = fs::read_dir(base_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

            // 접두어 매칭 (대소문자 무시)
            if prefix.is_empty() || name.to_lowercase().starts_with(&lower_prefix) {
                let display_name = if is_dir {
                    format!("{}/", name)
                } else {
                    name
                };
                suggestions.push((display_name, is_dir));
            }
        }
    }

    // 디렉토리 우선, 그 다음 이름순 정렬
    suggestions.sort_by(|a, b| {
        match (a.1, b.1) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
        }
    });

    suggestions.into_iter().map(|(name, _)| name).collect()
}

/// 자동완성 목록 업데이트 (입력할 때마다 호출)
/// 매칭되는 항목들을 목록에 표시
fn update_path_suggestions(dialog: &mut Dialog) {
    let (base_dir, prefix) = parse_path_for_completion(&dialog.input);
    let suggestions = get_path_suggestions(&base_dir, &prefix);

    if let Some(ref mut completion) = dialog.completion {
        if suggestions.is_empty() {
            completion.suggestions.clear();
            completion.visible = false;
        } else {
            completion.suggestions = suggestions;
            completion.selected_index = 0;
            completion.visible = true;
        }
    }
}

/// Tab 키로 자동완성 트리거
/// 유일 매칭: 바로 적용, 복수 매칭: 공통 접두어 적용
fn trigger_path_completion(dialog: &mut Dialog) {
    let (base_dir, prefix) = parse_path_for_completion(&dialog.input);
    let suggestions = get_path_suggestions(&base_dir, &prefix);

    if let Some(ref mut completion) = dialog.completion {
        if suggestions.is_empty() {
            completion.suggestions.clear();
            completion.visible = false;
        } else if suggestions.len() == 1 {
            // 유일 매칭 - 바로 적용
            apply_completion(dialog, &base_dir, &suggestions[0]);
            // 적용 후 새로운 suggestions 업데이트
            update_path_suggestions(dialog);
        } else {
            // 복수 매칭 - 공통 접두어 적용 후 목록 표시
            let common = find_common_prefix(&suggestions);
            if common.len() > prefix.len() {
                let new_path = base_dir.join(&common);
                dialog.input = new_path.display().to_string();
            }
            // 적용 후 새로운 suggestions 업데이트
            update_path_suggestions(dialog);
        }
    }
}

/// 공통 접두어 찾기
fn find_common_prefix(suggestions: &[String]) -> String {
    if suggestions.is_empty() {
        return String::new();
    }

    let first = &suggestions[0];
    let mut common_len = first.len();

    for s in suggestions.iter().skip(1) {
        let mut len = 0;
        for (c1, c2) in first.chars().zip(s.chars()) {
            if c1.to_lowercase().eq(c2.to_lowercase()) {
                len += c1.len_utf8();
            } else {
                break;
            }
        }
        common_len = common_len.min(len);
    }

    // 디렉토리 접미사 `/` 제외
    let common: String = first.chars().take(common_len).collect();
    common.trim_end_matches('/').to_string()
}

/// 선택된 자동완성 항목 적용
fn apply_completion(dialog: &mut Dialog, base_dir: &Path, suggestion: &str) {
    let new_path = base_dir.join(suggestion.trim_end_matches('/'));
    let mut path_str = new_path.display().to_string();

    // 디렉토리인 경우 `/` 추가
    if suggestion.ends_with('/') && !path_str.ends_with('/') {
        path_str.push('/');
    }

    dialog.input = path_str;
}

pub fn draw_dialog(frame: &mut Frame, dialog: &Dialog, area: Rect, theme: &Theme) {
    // 자동완성 목록이 표시될 경우 높이 동적 조정
    let completion_height = if let Some(ref completion) = dialog.completion {
        if completion.visible && !completion.suggestions.is_empty() {
            completion.suggestions.len().min(8) as u16 + 1
        } else {
            0
        }
    } else {
        0
    };

    // 다이얼로그 타입별 크기 설정
    let (width, height) = match dialog.dialog_type {
        DialogType::Delete => (50u16, 6u16),
        DialogType::Copy | DialogType::Move => {
            let w = area.width.saturating_sub(6).max(60);
            (w, 7 + completion_height)
        }
        DialogType::Goto => {
            // 터미널 너비 - 여백 6 (양쪽 3씩)
            let w = area.width.saturating_sub(6).max(60);
            (w, 6 + completion_height)
        }
        DialogType::Search | DialogType::Mkdir | DialogType::Rename => (50u16, 5u16),  // 간결한 입력창
    };

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    // Clear the area
    frame.render_widget(Clear, dialog_area);

    match dialog.dialog_type {
        DialogType::Delete => {
            draw_confirm_dialog(frame, dialog, dialog_area, theme);
        }
        DialogType::Copy | DialogType::Move => {
            draw_copy_move_dialog(frame, dialog, dialog_area, theme);
        }
        DialogType::Goto => {
            draw_goto_dialog(frame, dialog, dialog_area, theme);
        }
        DialogType::Search | DialogType::Mkdir | DialogType::Rename => {
            draw_simple_input_dialog(frame, dialog, dialog_area, theme);
        }
    }
}

/// 간결한 입력 다이얼로그 (Find File, Mkdir, Rename)
fn draw_simple_input_dialog(frame: &mut Frame, dialog: &Dialog, area: Rect, theme: &Theme) {
    let title = match dialog.dialog_type {
        DialogType::Search => " Find File ",
        DialogType::Mkdir => " Create Directory ",
        DialogType::Rename => " Rename ",
        _ => " Input ",
    };

    let block = Block::default()
        .title(title)
        .title_style(theme.header_style())
        .borders(Borders::ALL)
        .border_style(theme.border_style(true));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 입력 필드만 표시 (중앙 정렬)
    let max_input_width = (inner.width - 4) as usize;
    let input_chars: Vec<char> = dialog.input.chars().collect();
    let display_input = if input_chars.len() > max_input_width {
        let skip = input_chars.len().saturating_sub(max_input_width.saturating_sub(3));
        let suffix: String = input_chars[skip..].iter().collect();
        format!("...{}", suffix)
    } else {
        dialog.input.clone()
    };

    let input_line = Line::from(vec![
        Span::styled("> ", Style::default().fg(theme.info)),
        Span::styled(display_input, theme.normal_style()),
        Span::styled("_", Style::default().fg(theme.border_active).add_modifier(Modifier::SLOW_BLINK)),
    ]);

    // 수직 중앙에 배치
    let y_pos = inner.y + inner.height / 2;
    let input_area = Rect::new(inner.x + 1, y_pos, inner.width - 2, 1);
    frame.render_widget(Paragraph::new(input_line), input_area);
}

fn draw_confirm_dialog(frame: &mut Frame, dialog: &Dialog, area: Rect, theme: &Theme) {
    let block = Block::default()
        .title(" Delete ")
        .title_style(theme.header_style())
        .borders(Borders::ALL)
        .border_style(theme.border_style(true));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Message
    let message_area = Rect::new(inner.x + 1, inner.y + 1, inner.width - 2, 1);
    frame.render_widget(
        Paragraph::new(dialog.message.clone())
            .style(theme.normal_style())
            .alignment(ratatui::layout::Alignment::Center),
        message_area,
    );

    // 버튼 스타일
    let selected_style = theme.selected_style();
    let normal_style = theme.dim_style();

    let yes_style = if dialog.selected_button == 0 { selected_style } else { normal_style };
    let no_style = if dialog.selected_button == 1 { selected_style } else { normal_style };

    // 버튼 (중앙 정렬)
    let buttons = Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(" Yes ", yes_style),
        Span::styled("    ", Style::default()),
        Span::styled(" No ", no_style),
        Span::styled("  ", Style::default()),
    ]);
    let button_area = Rect::new(inner.x + 1, inner.y + inner.height - 2, inner.width - 2, 1);
    frame.render_widget(
        Paragraph::new(buttons).alignment(ratatui::layout::Alignment::Center),
        button_area,
    );
}

/// Copy/Move 다이얼로그 (경로 자동완성 포함)
fn draw_copy_move_dialog(frame: &mut Frame, dialog: &Dialog, area: Rect, theme: &Theme) {
    let title = match dialog.dialog_type {
        DialogType::Copy => " Copy ",
        DialogType::Move => " Move ",
        _ => " Transfer ",
    };

    let block = Block::default()
        .title(title)
        .title_style(theme.header_style())
        .borders(Borders::ALL)
        .border_style(theme.border_style(true));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 파일 목록 메시지
    let message_area = Rect::new(inner.x + 1, inner.y, inner.width - 2, 1);
    frame.render_widget(
        Paragraph::new(dialog.message.clone()).style(theme.normal_style()),
        message_area,
    );

    // 경로 입력 필드 (goto 스타일)
    let (base_dir, _) = parse_path_for_completion(&dialog.input);
    let is_root_path = base_dir == Path::new("/");

    let input_chars: Vec<char> = dialog.input.chars().collect();
    let prefix_char_start = if dialog.input.ends_with('/') {
        input_chars.len()
    } else {
        input_chars.iter().rposition(|&c| c == '/').map(|i| i + 1).unwrap_or(0)
    };

    let current_prefix: String = input_chars[prefix_char_start..].iter().collect();

    let preview_suffix = if let Some(ref completion) = dialog.completion {
        if completion.visible && !completion.suggestions.is_empty() {
            if let Some(selected) = completion.suggestions.get(completion.selected_index) {
                let selected_name = selected.trim_end_matches('/');
                if selected_name.to_lowercase().starts_with(&current_prefix.to_lowercase()) {
                    let suffix = &selected_name[current_prefix.len()..];
                    if selected.ends_with('/') {
                        format!("{}/", suffix)
                    } else {
                        suffix.to_string()
                    }
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let max_input_width = (inner.width - 4) as usize;
    let preview_chars: Vec<char> = preview_suffix.chars().collect();
    let total_len = input_chars.len() + preview_chars.len();

    let (display_input, display_preview, display_prefix_start) = if total_len > max_input_width {
        let available = max_input_width.saturating_sub(3);
        if preview_chars.len() >= available {
            let preview_display: String = preview_chars[..available].iter().collect();
            (String::from("..."), preview_display, 3)
        } else {
            let input_available = available - preview_chars.len();
            let skip = input_chars.len().saturating_sub(input_available);
            let input_display: String = input_chars[skip..].iter().collect();
            let prefix_pos = if prefix_char_start >= skip {
                3 + (prefix_char_start - skip)
            } else {
                3
            };
            (format!("...{}", input_display), preview_suffix.clone(), prefix_pos)
        }
    } else {
        (dialog.input.clone(), preview_suffix.clone(), prefix_char_start)
    };

    let input_line = Line::from(vec![
        Span::styled("> ", Style::default().fg(theme.info)),
        Span::styled(display_input, theme.normal_style()),
        Span::styled(&display_preview, theme.dim_style()),
        Span::styled("_", Style::default().fg(theme.border_active).add_modifier(Modifier::SLOW_BLINK)),
    ]);
    let input_area = Rect::new(inner.x + 1, inner.y + 2, inner.width - 2, 1);
    frame.render_widget(Paragraph::new(input_line), input_area);

    // 자동완성 목록 (입력창 아래 한 칸 여백)
    let list_start_y = inner.y + 4;
    let list_x = if is_root_path {
        inner.x + 1 + 2 + display_prefix_start as u16 - 1
    } else {
        inner.x + 1 + 2 + display_prefix_start as u16
    };
    let list_width = if is_root_path {
        inner.width.saturating_sub(2 + display_prefix_start as u16)
    } else {
        inner.width.saturating_sub(3 + display_prefix_start as u16)
    };

    if let Some(ref completion) = dialog.completion {
        if completion.visible && !completion.suggestions.is_empty() {
            draw_completion_list(
                frame,
                completion,
                Rect::new(list_x, list_start_y, list_width, inner.height.saturating_sub(6)),
                theme,
                is_root_path,
            );
        }
    }

    // 하단 도움말
    let help_line = Line::from(vec![
        Span::styled("Tab", theme.header_style()),
        Span::styled(":complete ", theme.dim_style()),
        Span::styled("Enter", theme.header_style()),
        Span::styled(":confirm ", theme.dim_style()),
        Span::styled("Esc", theme.header_style()),
        Span::styled(":cancel", theme.dim_style()),
    ]);
    let help_area = Rect::new(inner.x + 1, inner.y + inner.height - 1, inner.width - 2, 1);
    frame.render_widget(Paragraph::new(help_line), help_area);
}

#[allow(dead_code)]
fn draw_input_dialog(frame: &mut Frame, dialog: &Dialog, area: Rect, theme: &Theme) {
    let title = match dialog.dialog_type {
        DialogType::Mkdir => " Create Directory ",
        DialogType::Rename => " Rename File ",
        DialogType::Search => " Find File ",
        DialogType::Goto => " Go to Path ",
        _ => " Input ",
    };

    let block = Block::default()
        .title(title)
        .title_style(theme.header_style())
        .borders(Borders::ALL)
        .border_style(theme.border_style(true));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Prompt
    let prompt_area = Rect::new(inner.x + 1, inner.y + 1, inner.width - 2, 1);
    frame.render_widget(
        Paragraph::new(dialog.message.clone()).style(theme.normal_style()),
        prompt_area,
    );

    // Input field
    let max_input_width = (inner.width - 4) as usize;
    let input_chars: Vec<char> = dialog.input.chars().collect();
    let display_input = if input_chars.len() > max_input_width {
        let skip = input_chars.len().saturating_sub(max_input_width.saturating_sub(3));
        let suffix: String = input_chars[skip..].iter().collect();
        format!("...{}", suffix)
    } else {
        dialog.input.clone()
    };

    let input_line = Line::from(vec![
        Span::styled("> ", Style::default().fg(theme.info)),
        Span::styled(display_input, theme.normal_style()),
        Span::styled("_", Style::default().fg(theme.border_active).add_modifier(Modifier::SLOW_BLINK)),
    ]);
    let input_area = Rect::new(inner.x + 1, inner.y + 3, inner.width - 2, 1);
    frame.render_widget(Paragraph::new(input_line), input_area);

    // Help
    let help = Span::styled("[Enter] Confirm  [Esc] Cancel", theme.dim_style());
    let help_area = Rect::new(inner.x + 1, inner.y + inner.height - 2, inner.width - 2, 1);
    frame.render_widget(Paragraph::new(help), help_area);
}

/// Go to Path 대화상자 렌더링 (자동완성 목록 포함)
fn draw_goto_dialog(frame: &mut Frame, dialog: &Dialog, area: Rect, theme: &Theme) {
    let title = " Go to Path ";

    let block = Block::default()
        .title(title)
        .title_style(theme.header_style())
        .borders(Borders::ALL)
        .border_style(theme.border_style(true));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 입력에서 완성할 이름(prefix)의 시작 위치 계산 (char 인덱스)
    let input_chars: Vec<char> = dialog.input.chars().collect();
    let prefix_char_start = if dialog.input.ends_with('/') {
        input_chars.len()
    } else {
        // 마지막 '/' 위치 찾기
        input_chars.iter().rposition(|&c| c == '/').map(|i| i + 1).unwrap_or(0)
    };

    // 현재 입력된 prefix 추출
    let current_prefix: String = input_chars[prefix_char_start..].iter().collect();

    // base_dir 계산하여 루트 경로 여부 확인
    let (base_dir, _) = parse_path_for_completion(&dialog.input);
    let is_root_path = base_dir == Path::new("/");

    // 선택된 항목에서 미리보기 부분 계산 (입력된 prefix 이후 부분)
    let preview_suffix = if let Some(ref completion) = dialog.completion {
        if completion.visible && !completion.suggestions.is_empty() {
            if let Some(selected) = completion.suggestions.get(completion.selected_index) {
                let selected_name = selected.trim_end_matches('/');
                // 대소문자 무시하여 prefix 매칭 후 나머지 부분 추출
                if selected_name.to_lowercase().starts_with(&current_prefix.to_lowercase()) {
                    let suffix = &selected_name[current_prefix.len()..];
                    if selected.ends_with('/') {
                        format!("{}/", suffix)
                    } else {
                        suffix.to_string()
                    }
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Input field 및 표시 위치 계산
    // 미리보기를 포함한 전체 길이 고려
    let max_input_width = (inner.width - 4) as usize;
    let preview_chars: Vec<char> = preview_suffix.chars().collect();
    let total_len = input_chars.len() + preview_chars.len();

    let (display_input, display_preview, display_prefix_start) = if total_len > max_input_width {
        // 앞부분을 ...로 생략하고 뒷부분(미리보기 포함) 표시
        let available = max_input_width.saturating_sub(3); // "..." 제외한 공간

        if preview_chars.len() >= available {
            // 미리보기만으로도 공간 초과 - 미리보기만 잘라서 표시
            let preview_display: String = preview_chars[..available].iter().collect();
            (String::from("..."), preview_display, 3)
        } else {
            // 입력 일부 + 미리보기 전체 표시
            let input_available = available - preview_chars.len();
            let skip = input_chars.len().saturating_sub(input_available);
            let input_display: String = input_chars[skip..].iter().collect();
            let prefix_pos = if prefix_char_start >= skip {
                3 + (prefix_char_start - skip)
            } else {
                3
            };
            (format!("...{}", input_display), preview_suffix.clone(), prefix_pos)
        }
    } else {
        (dialog.input.clone(), preview_suffix.clone(), prefix_char_start)
    };

    // 입력 필드 렌더링 (선택된 항목 미리보기 포함)
    let input_line = Line::from(vec![
        Span::styled("> ", Style::default().fg(theme.info)),
        Span::styled(display_input, theme.normal_style()),
        Span::styled(&display_preview, theme.dim_style()),  // 흐리게 미리보기
        Span::styled("_", Style::default().fg(theme.border_active).add_modifier(Modifier::SLOW_BLINK)),
    ]);
    let input_area = Rect::new(inner.x + 1, inner.y + 1, inner.width - 2, 1);
    frame.render_widget(Paragraph::new(input_line), input_area);

    // 자동완성 목록 표시 (prefix 시작 위치에 맞춤)
    let list_start_y = inner.y + 2;
    // x 좌표: inner.x + 1 (패딩) + 2 ("> ") + prefix 시작 위치
    // 루트 경로일 때는 "/" 위치에 맞추기 위해 1 감소
    let list_x = if is_root_path {
        inner.x + 1 + 2 + display_prefix_start as u16 - 1
    } else {
        inner.x + 1 + 2 + display_prefix_start as u16
    };
    let list_width = if is_root_path {
        inner.width.saturating_sub(2 + display_prefix_start as u16)
    } else {
        inner.width.saturating_sub(3 + display_prefix_start as u16)
    };

    if let Some(ref completion) = dialog.completion {
        if completion.visible && !completion.suggestions.is_empty() {
            draw_completion_list(
                frame,
                completion,
                Rect::new(list_x, list_start_y, list_width, inner.height.saturating_sub(3)),
                theme,
                is_root_path,
            );
        }
    }

    // Help (맨 아래에 표시) - 메인 화면 스타일과 통일
    let help_line = if let Some(ref completion) = dialog.completion {
        if completion.visible && !completion.suggestions.is_empty() {
            Line::from(vec![
                Span::styled("↑↓", theme.header_style()),
                Span::styled(":select ", theme.dim_style()),
                Span::styled("Tab", theme.header_style()),
                Span::styled(":complete ", theme.dim_style()),
                Span::styled("Enter", theme.header_style()),
                Span::styled(":go ", theme.dim_style()),
                Span::styled("Esc", theme.header_style()),
                Span::styled(":cancel", theme.dim_style()),
            ])
        } else {
            Line::from(vec![
                Span::styled("Tab", theme.header_style()),
                Span::styled(":complete ", theme.dim_style()),
                Span::styled("Enter", theme.header_style()),
                Span::styled(":go ", theme.dim_style()),
                Span::styled("Esc", theme.header_style()),
                Span::styled(":cancel", theme.dim_style()),
            ])
        }
    } else {
        Line::from(vec![
            Span::styled("Enter", theme.header_style()),
            Span::styled(":confirm ", theme.dim_style()),
            Span::styled("Esc", theme.header_style()),
            Span::styled(":cancel", theme.dim_style()),
        ])
    };

    let help_area = Rect::new(inner.x + 1, inner.y + inner.height - 1, inner.width - 2, 1);
    frame.render_widget(Paragraph::new(help_line), help_area);
}

/// 자동완성 목록 렌더링
fn draw_completion_list(
    frame: &mut Frame,
    completion: &PathCompletion,
    area: Rect,
    theme: &Theme,
    is_root: bool,
) {
    let max_visible = area.height.min(8) as usize;
    let total = completion.suggestions.len();

    // 스크롤 계산: 선택된 항목이 항상 보이도록
    let scroll_offset = if total <= max_visible || completion.selected_index < max_visible / 2 {
        0
    } else if completion.selected_index >= total - max_visible / 2 {
        total - max_visible
    } else {
        completion.selected_index - max_visible / 2
    };

    let visible_items: Vec<&String> = completion
        .suggestions
        .iter()
        .skip(scroll_offset)
        .take(max_visible)
        .collect();

    let selected_style = Style::default()
        .bg(theme.bg_selected)
        .fg(theme.text_selected)
        .add_modifier(Modifier::BOLD);
    let dir_style = Style::default().fg(theme.text_directory);
    let file_style = theme.normal_style();

    for (i, suggestion) in visible_items.iter().enumerate() {
        let actual_index = scroll_offset + i;
        let is_selected = actual_index == completion.selected_index;
        let is_dir = suggestion.ends_with('/');

        let style = if is_selected {
            selected_style
        } else if is_dir {
            dir_style
        } else {
            file_style
        };

        // 루트 경로일 때 "/" 추가
        let display_name = if is_root {
            format!("/{}", suggestion)
        } else {
            suggestion.to_string()
        };

        // 전체 라인을 선택 스타일로 채우기
        let padded = format!("{:<width$}", display_name, width = area.width as usize);
        let line = Line::from(Span::styled(padded, style));

        let y = area.y + i as u16;
        if y < area.y + area.height {
            let item_area = Rect::new(area.x, y, area.width, 1);
            frame.render_widget(Paragraph::new(line), item_area);
        }
    }

    // 스크롤 인디케이터 (오른쪽에 표시)
    if total > max_visible {
        let scroll_info = format!("[{}/{}]", completion.selected_index + 1, total);
        let info_len = scroll_info.len() as u16;
        let info_x = area.x + area.width.saturating_sub(info_len + 1);
        let info_y = area.y;
        frame.render_widget(
            Paragraph::new(Span::styled(scroll_info, theme.dim_style())),
            Rect::new(info_x, info_y, info_len + 1, 1),
        );
    }
}

pub fn handle_dialog_input(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    if let Some(ref mut dialog) = app.dialog {
        match dialog.dialog_type {
            DialogType::Delete => {
                match code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        app.dialog = None;
                        app.execute_delete();
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        app.dialog = None;
                    }
                    KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                        // 버튼 토글 (0: Yes, 1: No)
                        dialog.selected_button = 1 - dialog.selected_button;
                    }
                    KeyCode::Enter => {
                        if dialog.selected_button == 0 {
                            app.dialog = None;
                            app.execute_delete();
                        } else {
                            app.dialog = None;
                        }
                    }
                    _ => {}
                }
            }
            DialogType::Copy | DialogType::Move => {
                return handle_copy_move_dialog_input(app, code, modifiers);
            }
            DialogType::Goto => {
                return handle_goto_dialog_input(app, code, modifiers);
            }
            _ => {
                match code {
                    KeyCode::Enter => {
                        let input = dialog.input.clone();
                        let dialog_type = dialog.dialog_type;
                        app.dialog = None;
                        if !input.trim().is_empty() {
                            match dialog_type {
                                DialogType::Mkdir => app.execute_mkdir(&input),
                                DialogType::Rename => app.execute_rename(&input),
                                DialogType::Search => app.execute_search(&input),
                                DialogType::Goto => app.execute_goto(&input),
                                _ => {}
                            }
                        }
                    }
                    KeyCode::Esc => {
                        app.dialog = None;
                    }
                    KeyCode::Backspace => {
                        dialog.input.pop();
                    }
                    KeyCode::Char(c) => {
                        dialog.input.push(c);
                    }
                    _ => {}
                }
            }
        }
    }
    false
}

/// Go to Path 대화상자 키 입력 처리
fn handle_goto_dialog_input(app: &mut App, code: KeyCode, _modifiers: KeyModifiers) -> bool {
    if let Some(ref mut dialog) = app.dialog {
        let completion_visible = dialog
            .completion
            .as_ref()
            .map(|c| c.visible && !c.suggestions.is_empty())
            .unwrap_or(false);

        match code {
            KeyCode::Tab => {
                if completion_visible {
                    // 목록에서 선택된 항목으로 완성
                    let (base_dir, _) = parse_path_for_completion(&dialog.input);
                    let suggestion = dialog
                        .completion
                        .as_ref()
                        .and_then(|c| c.suggestions.get(c.selected_index).cloned());

                    if let Some(suggestion) = suggestion {
                        apply_completion(dialog, &base_dir, &suggestion);
                    }
                    // 완성 후 새로운 suggestions 업데이트
                    update_path_suggestions(dialog);
                } else {
                    // 목록이 없으면 자동완성 트리거
                    trigger_path_completion(dialog);
                }
            }
            KeyCode::BackTab => {
                // Shift+Tab: 이전 항목
                if completion_visible {
                    if let Some(ref mut completion) = dialog.completion {
                        if !completion.suggestions.is_empty() {
                            if completion.selected_index == 0 {
                                completion.selected_index = completion.suggestions.len() - 1;
                            } else {
                                completion.selected_index -= 1;
                            }
                        }
                    }
                }
            }
            KeyCode::Up => {
                if completion_visible {
                    if let Some(ref mut completion) = dialog.completion {
                        if !completion.suggestions.is_empty() {
                            if completion.selected_index == 0 {
                                completion.selected_index = completion.suggestions.len() - 1;
                            } else {
                                completion.selected_index -= 1;
                            }
                        }
                    }
                }
            }
            KeyCode::Down => {
                if completion_visible {
                    if let Some(ref mut completion) = dialog.completion {
                        if !completion.suggestions.is_empty() {
                            completion.selected_index =
                                (completion.selected_index + 1) % completion.suggestions.len();
                        }
                    }
                }
            }
            KeyCode::Enter => {
                if completion_visible {
                    // 선택된 항목으로 완성
                    let (base_dir, _) = parse_path_for_completion(&dialog.input);
                    let suggestion = dialog
                        .completion
                        .as_ref()
                        .and_then(|c| c.suggestions.get(c.selected_index).cloned());

                    if let Some(suggestion) = suggestion {
                        apply_completion(dialog, &base_dir, &suggestion);
                    }
                }

                // 경로 검증
                let input = dialog.input.clone();
                if input.trim().is_empty() {
                    return false;
                }

                let path = expand_path_string(&input);

                if !path.exists() {
                    // 존재하지 않는 경로 - 다이얼로그 유지, 하단에 에러 메시지 표시
                    // 자동완성 목록 숨기기
                    if let Some(ref mut completion) = dialog.completion {
                        completion.visible = false;
                        completion.suggestions.clear();
                    }
                    let error_msg = format!("Path not found: {}", input);
                    app.show_message(&error_msg);
                    return false;
                }

                if path.is_file() {
                    // 파일인 경우 - 부모 디렉토리로 이동하고 파일에 커서 위치
                    if let Some(parent) = path.parent() {
                        let filename = path.file_name()
                            .map(|n| n.to_string_lossy().to_string());
                        app.dialog = None;
                        app.goto_directory_with_focus(parent, filename);
                        app.show_message(&format!("Moved to file: {}", path.display()));
                    }
                    return false;
                }

                // 디렉토리인 경우 - 그 디렉토리로 이동
                app.dialog = None;
                app.execute_goto(&input);
                return false;
            }
            KeyCode::Esc => {
                if completion_visible {
                    // 목록 숨기기
                    if let Some(ref mut completion) = dialog.completion {
                        completion.visible = false;
                        completion.suggestions.clear();
                    }
                } else {
                    // 다이얼로그 닫기
                    app.dialog = None;
                }
            }
            KeyCode::Backspace => {
                dialog.input.pop();
                // 입력 변경 후 자동완성 목록 업데이트
                update_path_suggestions(dialog);
            }
            KeyCode::Char(c) => {
                if c == '/' && completion_visible {
                    // '/' 입력 시 선택된 항목으로 완성 (Tab과 동일)
                    let (base_dir, _) = parse_path_for_completion(&dialog.input);
                    let suggestion = dialog
                        .completion
                        .as_ref()
                        .and_then(|comp| comp.suggestions.get(comp.selected_index).cloned());

                    if let Some(suggestion) = suggestion {
                        apply_completion(dialog, &base_dir, &suggestion);
                    }
                    // 완성 후 새로운 suggestions 업데이트
                    update_path_suggestions(dialog);
                } else if c == '~' {
                    // '~' 입력 시 홈 폴더 경로로 설정
                    if let Some(home) = dirs::home_dir() {
                        dialog.input = format!("{}/", home.display());
                        update_path_suggestions(dialog);
                    }
                } else {
                    dialog.input.push(c);
                    // 입력 변경 후 자동완성 목록 업데이트
                    update_path_suggestions(dialog);
                }
            }
            _ => {}
        }
    }
    false
}

/// Copy/Move 다이얼로그 키 입력 처리
fn handle_copy_move_dialog_input(app: &mut App, code: KeyCode, _modifiers: KeyModifiers) -> bool {
    if let Some(ref mut dialog) = app.dialog {
        let completion_visible = dialog
            .completion
            .as_ref()
            .map(|c| c.visible && !c.suggestions.is_empty())
            .unwrap_or(false);

        match code {
            KeyCode::Tab => {
                if completion_visible {
                    let (base_dir, _) = parse_path_for_completion(&dialog.input);
                    let suggestion = dialog
                        .completion
                        .as_ref()
                        .and_then(|c| c.suggestions.get(c.selected_index).cloned());

                    if let Some(suggestion) = suggestion {
                        apply_completion(dialog, &base_dir, &suggestion);
                    }
                    update_path_suggestions(dialog);
                } else {
                    trigger_path_completion(dialog);
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if completion_visible {
                    if let Some(ref mut completion) = dialog.completion {
                        if !completion.suggestions.is_empty() {
                            if completion.selected_index == 0 {
                                completion.selected_index = completion.suggestions.len() - 1;
                            } else {
                                completion.selected_index -= 1;
                            }
                        }
                    }
                }
            }
            KeyCode::Down => {
                if completion_visible {
                    if let Some(ref mut completion) = dialog.completion {
                        if !completion.suggestions.is_empty() {
                            completion.selected_index =
                                (completion.selected_index + 1) % completion.suggestions.len();
                        }
                    }
                }
            }
            KeyCode::Enter => {
                if completion_visible {
                    let (base_dir, _) = parse_path_for_completion(&dialog.input);
                    let suggestion = dialog
                        .completion
                        .as_ref()
                        .and_then(|c| c.suggestions.get(c.selected_index).cloned());

                    if let Some(suggestion) = suggestion {
                        apply_completion(dialog, &base_dir, &suggestion);
                    }
                    update_path_suggestions(dialog);
                    return false;
                }

                // 경로 검증
                let input = dialog.input.clone();
                if input.trim().is_empty() {
                    app.show_message("Please enter a target path");
                    return false;
                }

                let path = expand_path_string(&input);

                if !path.exists() || !path.is_dir() {
                    if let Some(ref mut completion) = dialog.completion {
                        completion.visible = false;
                        completion.suggestions.clear();
                    }
                    app.show_message(&format!("Invalid directory: {}", input));
                    return false;
                }

                // 복사/이동 실행
                let dialog_type = dialog.dialog_type;
                let target_path = path;
                app.dialog = None;

                match dialog_type {
                    DialogType::Copy => app.execute_copy_to(&target_path),
                    DialogType::Move => app.execute_move_to(&target_path),
                    _ => {}
                }
                return false;
            }
            KeyCode::Esc => {
                if completion_visible {
                    if let Some(ref mut completion) = dialog.completion {
                        completion.visible = false;
                        completion.suggestions.clear();
                    }
                } else {
                    app.dialog = None;
                }
            }
            KeyCode::Backspace => {
                dialog.input.pop();
                update_path_suggestions(dialog);
            }
            KeyCode::Char(c) => {
                if c == '/' && completion_visible {
                    let (base_dir, _) = parse_path_for_completion(&dialog.input);
                    let suggestion = dialog
                        .completion
                        .as_ref()
                        .and_then(|comp| comp.suggestions.get(comp.selected_index).cloned());

                    if let Some(suggestion) = suggestion {
                        apply_completion(dialog, &base_dir, &suggestion);
                    }
                    update_path_suggestions(dialog);
                } else if c == '~' {
                    if let Some(home) = dirs::home_dir() {
                        dialog.input = format!("{}/", home.display());
                        update_path_suggestions(dialog);
                    }
                } else {
                    dialog.input.push(c);
                    update_path_suggestions(dialog);
                }
            }
            _ => {}
        }
    }
    false
}
