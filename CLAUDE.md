You can get Rust build methods from build_manual.md file

## CRITICAL: Do Not Change Design Without Permission

- **NEVER change product design/UX without explicit user request**
- Bug fix and design change are completely different things
- If you identify a "potential improvement" or "UX issue", only REPORT it - do NOT implement
- When user says "fix it", fix only the BUGS, not your suggestions
- If you think design change is needed, ASK FIRST before implementing
- Violating this rule wastes user's time and breaks trust

## Build Guidelines

- **IMPORTANT: Only build when the user explicitly requests it**
- Never run build commands automatically after code changes
- Never run build commands to "verify" or "check" code
- Do not use `cargo build`, `python3 build.py`, or any build commands unless user asks
- Focus only on code modifications; user handles all builds manually

## Version Management

- Version is defined in `Cargo.toml` (line 3: `version = "x.x.x"`)
- All version displays use `env!("CARGO_PKG_VERSION")` macro to read from Cargo.toml
- To update version: modify `Cargo.toml` and synchronize the root `cokacdir` package version in `Cargo.lock`; version displays reflect `Cargo.toml` automatically
- Keep `Cargo.lock` in version control because release builds use `--locked`. A version-only bump must preserve all dependency versions; do not remove `--locked` to bypass a stale lock file
- Never hardcode version strings in source code

## Theme Color System

- All color definitions must use `Color::Indexed(number)` format directly
- Each UI element must have its own uniquely named color field, even if the color value is the same as another element
- Never reference another element's color (e.g., don't use `theme.bg_selected` for viewer search input)
- Define dedicated color fields in the appropriate Colors struct (e.g., `ViewerColors.search_input_text`)
- Color values may be duplicated across fields, but names must be unique and semantically meaningful

### Theme File Locations

- **Source of truth**: `src/ui/theme.rs` - 테마 색상 값과 JSON 주석 모두 이 파일에서 정의
- **Generated files**: `~/.cokacdir/themes/*.json` - 프로그램 실행 시 생성되는 사용자 설정 파일
- 테마 수정 시 반드시 `src/ui/theme.rs`를 수정해야 함 (생성된 JSON 파일 직접 수정 금지)
- JSON 주석 형식: `"__field__": "설명"` - 이 주석들도 theme.rs의 `to_json()` 함수 내에 정의됨
