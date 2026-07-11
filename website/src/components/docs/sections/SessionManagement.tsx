import { SectionTitle, SubSection, P, IC, InfoBox, CodeBlock, CommandTable } from '../DocComponents'
import { useLanguage } from '../../LanguageContext'

export default function SessionManagement() {
  const { t } = useLanguage()
  return (
    <div>
      <SectionTitle>{t('Session Management', '세션 관리')}</SectionTitle>
      <P>{t(<>Manage sessions with <IC>/start</IC>, <IC>/session</IC>, and <IC>/clear</IC>.</>, <><IC>/start</IC>, <IC>/session</IC>, <IC>/clear</IC>로 세션을 관리하세요.</>)}</P>

      <SubSection title={String(t('/start (no argument)', '/start (인자 없음)'))}>
        <P>{t(
          <>Creates a workspace at <IC>~/.cokacdir/workspace/&lt;random_id&gt;</IC> with an 8-character random ID. The ID serves as a shortcut — you can resume with <IC>/&lt;id&gt;</IC>.</>,
          <><IC>~/.cokacdir/workspace/&lt;random_id&gt;</IC>에 8자 랜덤 ID로 워크스페이스를 생성합니다. ID는 단축키로 사용 가능 — <IC>/&lt;id&gt;</IC>로 재개할 수 있습니다.</>
        )}</P>
      </SubSection>

      <SubSection title={String(t('/start <path>', '/start <경로>'))}>
        <P>{t(
          'Starts a session at the specified filesystem path. Creates the directory if it doesn\'t exist.',
          '지정한 파일시스템 경로에서 세션을 시작합니다. 디렉토리가 없으면 생성합니다.'
        )}</P>
        <P>{t(
          <> Recognized path formats: paths starting with <IC>/</IC>, <IC>~</IC>, <IC>.</IC>, or Windows drive letters.</>,
          <>인식되는 경로 형식: <IC>/</IC>, <IC>~</IC>, <IC>.</IC>으로 시작하거나 Windows 드라이브 문자로 시작하는 경로.</>
        )}</P>
        <CodeBlock code="/start ~/projects/my-app" />
      </SubSection>

      <SubSection title={String(t('/start <session_id or name>', '/start <세션_id 또는 이름>'))}>
        <P>{t(
          'Resolves a session by UUID or name across all providers (Claude, Codex, Agy, OpenCode). Automatically switches model if the session belongs to a different provider.',
          '모든 제공자(Claude, Codex, Agy, OpenCode)에서 UUID 또는 이름으로 세션을 찾습니다. 세션이 다른 제공자에 속하면 자동으로 모델을 전환합니다.'
        )}</P>
      </SubSection>

      <SubSection title={String(t('Session Lifecycle', '세션 라이프사이클'))}>
        <ol className="list-decimal list-inside space-y-2 text-zinc-400 my-4 ml-2">
          <li>{t(<>After <IC>/start</IC>, a session exists locally without an ID</>, <><IC>/start</IC> 후 세션이 ID 없이 로컬에 생성됩니다</>)}</li>
          <li>{t('First message creates the actual session with a UUID', '첫 번째 메시지가 UUID가 있는 실제 세션을 생성합니다')}</li>
          <li>{t(<>Session is saved to <IC>~/.cokacdir/ai_sessions/&lt;session_id&gt;.json</IC></>, <>세션이 <IC>~/.cokacdir/ai_sessions/&lt;session_id&gt;.json</IC>에 저장됩니다</>)}</li>
          <li>{t('Subsequent messages maintain conversation history in the same session', '이후 메시지는 같은 세션에서 대화 기록을 유지합니다')}</li>
        </ol>
      </SubSection>

      <SubSection title={String(t('Session Restoration', '세션 복원'))}>
        <P>{t(
          'Opening an existing directory automatically restores the previous session, including conversation history and session ID. A preview of recent messages is shown.',
          '기존 디렉토리를 열면 대화 기록과 세션 ID를 포함하여 이전 세션이 자동으로 복원됩니다. 최근 메시지 미리보기가 표시됩니다.'
        )}</P>
        <InfoBox type="tip">
          {t(
            <> Use <IC>/&lt;id&gt;</IC> as a shortcut instead of <IC>/start &lt;full_path&gt;</IC> for workspace directories.</>,
            <>워크스페이스 디렉토리의 경우 <IC>/start &lt;전체_경로&gt;</IC> 대신 <IC>/&lt;id&gt;</IC>를 단축키로 사용하세요.</>
          )}
        </InfoBox>
      </SubSection>

      <SubSection title={String(t('Auto-Restore on Restart', '재시작 시 자동 복원'))}>
        <P>{t(
          'The bot remembers the last active path per chat and auto-restores it when the server restarts.',
          '봇은 채팅별 마지막 활성 경로를 기억하고 서버 재시작 시 자동으로 복원합니다.'
        )}</P>
      </SubSection>

      <SubSection title="/session">
        <P>{t(
          'Displays current session info: UUID, working directory, and the CLI resume command. Shows "No active session" if none exists.',
          '현재 세션 정보를 표시합니다: UUID, 작업 디렉토리, CLI 재개 명령어. 세션이 없으면 "No active session"을 표시합니다.'
        )}</P>
      </SubSection>

      <SubSection title={String(t('Resume Commands by Provider', '제공자별 재개 명령어'))}>
        <CommandTable
          headers={[String(t('Provider', '제공자')), String(t('Command', '명령어'))]}
          rows={[
            ['Claude', 'claude --resume <session_id>'],
            ['Codex', 'codex resume <session_id>'],
            ['Agy', 'agy --conversation <session_id>'],
            ['OpenCode', 'opencode -s <session_id>'],
          ]}
        />
      </SubSection>

      <SubSection title="/clear">
        <P>{t('Discards the current session but preserves the working directory.', '현재 세션을 폐기하지만 작업 디렉토리는 보존합니다.')}</P>
        <ul className="list-disc list-inside space-y-1.5 text-zinc-400 my-4 ml-2">
          <li>{t(<>Sets session ID to None</>, <>세션 ID를 None으로 설정</>)}</li>
          <li>{t('Clears conversation history and pending file uploads', '대화 기록과 대기 중인 파일 업로드를 삭제')}</li>
          <li>{t('Overwrites session file with minimal data', '세션 파일을 최소 데이터로 덮어쓰기')}</li>
          <li>{t(<>Does <strong className="text-zinc-300">NOT</strong> delete workspace directory or files</>, <>워크스페이스 디렉토리나 파일은 <strong className="text-zinc-300">삭제하지 않습니다</strong></>)}</li>
          <li>{t(<>Does <strong className="text-zinc-300">NOT</strong> stop running requests</>, <>실행 중인 요청은 <strong className="text-zinc-300">중지하지 않습니다</strong></>)}</li>
        </ul>
        <P>{t('The next message creates a brand new session with a new UUID.', '다음 메시지가 새 UUID로 완전히 새로운 세션을 생성합니다.')}</P>
      </SubSection>
    </div>
  )
}
