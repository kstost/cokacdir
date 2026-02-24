// Korean localization strings.

// Session
pub(super) const MSG_NO_SESSION: &str =
    "활성 세션이 없습니다. /start <경로> 로 시작하세요.";
pub(super) const MSG_SESSION_CLEARED: &str = "세션이 초기화되었습니다.";

// AI busy
pub(super) const MSG_AI_BUSY: &str = "AI 요청이 진행 중입니다. /stop 으로 중단할 수 있습니다.";

// Permission
pub(super) const MSG_PERMISSION_DENIED: &str = "권한 없음.";

// Stop
pub(super) const MSG_STOPPING: &str = "중단 중...";
pub(super) const MSG_NO_ACTIVE_REQUEST: &str = "중단할 활성 요청이 없습니다.";

// Public
pub(super) const MSG_GROUP_ONLY: &str = "이 명령은 그룹 채팅에서만 사용할 수 있습니다.";
pub(super) const MSG_PUBLIC_OWNER_ONLY: &str =
    "봇 소유자만 public 접근 설정을 변경할 수 있습니다.";
pub(super) const MSG_PUBLIC_ON: &str =
    "✅ 이 그룹의 Public 접근이 <b>활성화</b>되었습니다.\n모든 멤버가 봇을 사용할 수 있습니다.";
pub(super) const MSG_PUBLIC_OFF: &str =
    "❌ 이 그룹의 Public 접근이 <b>비활성화</b>되었습니다.\n소유자만 봇을 사용할 수 있습니다.";
pub(super) const MSG_PUBLIC_STATUS_ENABLED: &str = "활성화됨";
pub(super) const MSG_PUBLIC_STATUS_DISABLED: &str = "비활성화됨";
pub(super) const MSG_PUBLIC_STATUS: &str =
    "이 그룹의 Public 접근 상태: <b>{}</b>\n\n<code>/public on</code> — 모든 멤버 허용\n<code>/public off</code> — 소유자만";
pub(super) const MSG_PUBLIC_USAGE: &str =
    "사용법:\n<code>/public on</code> — 모든 그룹 멤버 허용\n<code>/public off</code> — 소유자만";

// Language
pub(super) const MSG_LANG_CHANGED: &str = "✅ 언어가 <b>한국어</b>로 설정되었습니다.";
pub(super) const MSG_LANG_USAGE: &str =
    "사용법: <code>/lang ko</code> 또는 <code>/lang en</code>";

// Shell
pub(super) const MSG_SHELL_USAGE: &str = "사용법: !<명령어>\n예시: !mkdir /home/user/testcode";
pub(super) const MSG_SHELL_TIMEOUT: &str = "명령 실행 시간 초과 (60초 제한)";
pub(super) const MSG_SHELL_PROCESSING: &str = "{}에 대해서 처리중입니다.";

// Down (file download)
pub(super) const MSG_DOWN_USAGE: &str =
    "사용법: /down <파일경로>\n예시: /down /home/user/file.txt";
pub(super) const MSG_DOWN_NO_SESSION: &str =
    "활성 세션이 없습니다. 절대 경로를 사용하거나 먼저 /start <경로>로 시작하세요.";

// File ops
pub(super) const MSG_FILE_SAVE_FAILED: &str = "파일 저장 실패: {}";
pub(super) const MSG_SANDBOX_DENIED: &str =
    "오류: 경로가 허용된 샌드박스(홈 디렉토리) 밖에 있습니다.";

// Errors
pub(super) const MSG_ERROR_HOME: &str = "오류: 홈 디렉토리를 확인할 수 없습니다.";
pub(super) const MSG_ERROR_INVALID_DIR: &str = "오류: '{}' 은(는) 유효한 디렉토리가 아닙니다.";
pub(super) const MSG_ERROR_CREATE_WORKSPACE: &str = "오류: 워크스페이스 생성 실패: {}";

// Help text
pub(super) fn help_text() -> &'static str {
    "\
<b>cokacdir Telegram Bot</b>
서버 파일 관리 &amp; Claude AI 대화

<b>세션</b>
<code>/start &lt;경로&gt;</code> — 디렉토리에서 세션 시작
<code>/start</code> — 자동 생성 워크스페이스로 시작
<code>/pwd</code> — 현재 작업 경로 확인
<code>/clear</code> — AI 대화 기록 삭제
<code>/stop</code> — 현재 AI 요청 중단

<b>파일 전송</b>
<code>/down &lt;파일&gt;</code> — 서버 파일 다운로드
파일/사진 전송 — 세션 디렉토리에 업로드

<b>셸 명령어</b>
<code>!&lt;명령어&gt;</code> — 셸 명령어 직접 실행
  예: <code>!ls -la</code>, <code>!git status</code>

<b>AI 대화</b>
다른 메시지는 Claude AI 에게 전송됩니다.
AI는 세션의 파일을 읽고, 수정하고, 명령을 실행할 수 있습니다.

<b>도구 관리</b>
<code>/availabletools</code> — 전체 도구 목록
<code>/allowedtools</code> — 현재 허용된 도구 목록
<code>/allowed +이름</code> — 도구 추가 (예: <code>/allowed +Bash</code>)
<code>/allowed -이름</code> — 도구 제거
<code>/allowed +a -b +c</code> — 여러 개 동시에

<b>그룹 채팅</b>
<code>;</code><i>메시지</i> — AI에게 메시지 전송
<code>;</code><i>캡션</i> — AI 프롬프트와 함께 파일 업로드
<code>/public on</code> — 모든 멤버 사용 허용
<code>/public off</code> — 소유자만 (기본값)

<b>언어</b>
<code>/lang ko</code> — 한국어
<code>/lang en</code> — English

<b>설정</b>
<code>/setpollingtime &lt;ms&gt;</code> — API 폴링 간격 설정
  너무 낮으면 Telegram API 속도 제한이 발생할 수 있습니다.
  최소 2500ms, 권장 3000ms 이상.
<code>/debug</code> — API 디버그 로깅 토글

<code>/help</code> — 이 도움말 표시"
}
