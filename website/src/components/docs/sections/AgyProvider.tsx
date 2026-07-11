import { SectionTitle, SubSection, P, IC, CodeBlock, CommandTable, InfoBox, UL } from '../DocComponents'
import { useLanguage } from '../../LanguageContext'

export default function AgyProvider() {
  const { t } = useLanguage()
  return (
    <div>
      <SectionTitle>{t('Agy Provider', 'Agy 프로바이더')}</SectionTitle>
      <P>{t(
        <>cokacdir integrates with Google Antigravity CLI through the local <IC>agy</IC> binary. The implementation is based on measured CLI behavior, not on a JSON event protocol.</>,
        <>cokacdir는 로컬 <IC>agy</IC> 바이너리를 통해 Google Antigravity CLI와 연동합니다. 이 구현은 JSON 이벤트 프로토콜이 아니라 실제 CLI 동작 실측에 기반합니다.</>
      )}</P>

      <SubSection title={String(t('Invocation', '실행 방식'))}>
        <P>{t(
          <>Only the current user request is written to non-TTY stdin. On Linux, the complete system prompt is injected separately as a transient system message through Agy's official <IC>PreInvocation</IC> hook.</>,
          <>non-TTY stdin에는 현재 사용자 요청만 전달합니다. Linux에서는 전체 시스템 프롬프트를 Agy 공식 <IC>PreInvocation</IC> 훅을 통해 별도의 일시적 시스템 메시지로 주입합니다.</>
        )}</P>
        <CodeBlock code={'agy --print-timeout <duration> --log-file ~/.cokacdir/tmp/<private-log> --dangerously-skip-permissions'} />
        <P>{t(
          <>cokacdir installs one namespaced global Agy plugin under <IC>~/.gemini/config/plugins/</IC>. For each run, the full system prompt is stored in an owner-only <IC>~/.cokacdir/tmp/agy_system_prompt_*</IC> file and returned by the hook as an <IC>ephemeralMessage</IC>.</>,
          <>cokacdir는 <IC>~/.gemini/config/plugins/</IC> 아래에 이름이 분리된 전역 Agy 플러그인 하나를 설치합니다. 실행마다 전체 시스템 프롬프트를 소유자 전용 <IC>~/.cokacdir/tmp/agy_system_prompt_*</IC> 파일에 두고 훅이 <IC>ephemeralMessage</IC>로 반환합니다.</>
        )}</P>
        <P>{t(
          <>No <IC>--print</IC>, <IC>-p</IC>, or <IC>--prompt</IC> flag is used. Agy 1.1.1 reads piped stdin when prompt flags are absent; a flag prompt disables stdin prompt reading.</>,
          <><IC>--print</IC>, <IC>-p</IC>, <IC>--prompt</IC> 플래그는 사용하지 않습니다. Agy 1.1.1은 프롬프트 플래그가 없을 때 파이프 stdin을 읽으며, 플래그 프롬프트를 주면 stdin 프롬프트를 읽지 않습니다.</>
        )}</P>
        <P>{t(
          <>Resume uses a fresh file with the current complete system prompt plus <IC>--conversation &lt;session_id&gt;</IC>. No <IC>--add-dir</IC> is used, so the project, <IC>AGENTS.md</IC>, and active workspace are unchanged.</>,
          <>세션 재개 때도 현재의 전체 시스템 프롬프트를 새 파일에 담고 <IC>--conversation &lt;session_id&gt;</IC>를 사용합니다. <IC>--add-dir</IC>를 쓰지 않으므로 프로젝트, <IC>AGENTS.md</IC>, 실제 작업공간은 바뀌지 않습니다.</>
        )}</P>
        <P>{t(
          <>Every hook call records a private start/success ledger entry. cokacdir buffers all Agy output until every entry is complete, terminates an incomplete hook after 30 seconds, and removes prompt files after exit. Locks let the next run safely remove files left by a crash without touching active runs.</>,
          <>모든 훅 호출은 비공개 시작/성공 ledger를 기록합니다. cokacdir는 모든 항목이 완료될 때까지 Agy 출력을 보류하고, 30초 동안 완료되지 않은 훅은 종료하며, 종료 뒤 프롬프트 파일을 삭제합니다. 잠금으로 실행 중인 파일을 구분해 다음 실행에서 crash 잔여 파일도 안전하게 정리합니다.</>
        )}</P>
        <InfoBox type="info">
          {t(
            <>The hook transport is enabled only on verified Linux builds. Windows and macOS currently use the legacy combined-stdin fallback. The installed global plugin returns no injected message when cokacdir's private per-process environment is absent.</>,
            <>훅 전송은 검증된 Linux 빌드에서만 활성화됩니다. Windows와 macOS는 현재 기존의 합성 stdin 방식으로 대체합니다. 설치된 전역 플러그인은 cokacdir의 실행별 비공개 환경이 없으면 아무 메시지도 주입하지 않습니다.</>
          )}
        </InfoBox>
      </SubSection>

      <SubSection title={String(t('Stdout Contract', 'stdout 규약'))}>
        <P>{t(
          <>Agy headless stdin mode emits plain stdout. It does not emit structured tool-use events like Claude or Codex JSONL streams.</>,
          <>Agy headless stdin 모드는 평문 stdout을 출력합니다. Claude나 Codex의 JSONL 스트림처럼 구조화된 도구 사용 이벤트를 내보내지 않습니다.</>
        )}</P>
        <UL>
          <li>{t('Successful output may be only the final answer.', '성공 출력은 최종 답변만 포함할 수 있습니다.')}</li>
          <li>{t('Successful output may include narration before the final answer.', '성공 출력은 최종 답변 전에 진행 설명을 포함할 수 있습니다.')}</li>
          <li>{t('Resume output may replay previous assistant stdout before new text.', 'resume 출력은 새 텍스트 앞에 이전 assistant stdout을 다시 포함할 수 있습니다.')}</li>
          <li>{t('stderr is usually empty, even when the real failure is in the Agy log.', '실제 실패가 Agy 로그에만 있어도 stderr는 대체로 비어 있습니다.')}</li>
        </UL>
      </SubSection>

      <SubSection title={String(t('Measured Failure Modes', '실측된 실패 형태'))}>
        <CommandTable
          headers={[String(t('Shape', '형태')), String(t('cokacdir handling', 'cokacdir 처리'))]}
          rows={[
            ['Error: timed out waiting for response', String(t('Treated as fatal even with exit code 0.', 'exit code 0이어도 fatal로 처리합니다.'))],
            ['Warning: conversation "<id>" not found.', String(t('Prevalidated and treated as fatal if it appears.', '사전 검증하며, 출력에 나타나면 fatal로 처리합니다.'))],
            ['exit 0 + empty stdout/stderr + RESOURCE_EXHAUSTED in log', String(t('Reported as quota exhaustion by reading the per-run log file.', '실행별 로그 파일을 읽어 quota 초과로 보고합니다.'))],
            ['startup "not logged" followed by silent auth success', String(t('Ignored as transient authentication startup noise.', '일시적인 인증 초기화 로그로 보고 무시합니다.'))],
          ]}
        />
        <InfoBox type="info">
          {t(
            <>cokacdir sets a dedicated <IC>--log-file</IC> for every Agy run. Log summaries are surfaced only when the process fails or when no visible stdout was produced.</>,
            <>cokacdir는 Agy 실행마다 전용 <IC>--log-file</IC>을 지정합니다. 로그 요약은 프로세스가 실패했거나 보이는 stdout이 없을 때만 사용자에게 노출합니다.</>
          )}
        </InfoBox>
      </SubSection>

      <SubSection title={String(t('Measured Tool Probes', '실측된 도구 probe'))}>
        <P>{t(
          'After quota recovered on 2026-06-17, live probes succeeded for filesystem read/write/edit, shell commands, grep, web/read-url/search, browser, subagent, MCP availability checks, and skill/knowledge checks.',
          '2026-06-17 quota 회복 후 파일 읽기/쓰기/수정, 셸 명령, grep, web/read-url/search, browser, subagent, MCP 가용성 확인, skill/knowledge 확인 probe가 실제 stdout으로 성공했습니다.'
        )}</P>
        <CommandTable
          headers={[String(t('Probe', 'probe')), String(t('Observed stdout', '관측 stdout'))]}
          rows={[
            ['filesystem list/read', 'FS_READ_OK 4'],
            ['filesystem write/edit', 'FS_WRITE_OK'],
            ['shell command', 'SHELL_OK'],
            ['grep/search', 'GREP_OK .../src/input.txt'],
            ['web/read-url/search', 'WEB_OK example.com'],
            ['browser', 'BROWSER_OK Example Domain'],
            ['subagent', 'SUBAGENT_OK subagent-pong'],
            ['MCP', 'MCP_OK none'],
            ['skill/knowledge', 'SKILL_KNOWLEDGE_OK 9'],
          ]}
        />
      </SubSection>

      <SubSection title={String(t('Limitations', '제한 사항'))}>
        <UL>
          <li>{t(<><IC>/allowed</IC> tool restrictions do not constrain Agy; they are enforced only for Claude.</>, <><IC>/allowed</IC> 도구 제한은 Agy를 제약하지 않습니다. 현재 Claude에만 적용됩니다.</>)}</li>
          <li>{t(<><IC>/loop</IC> verification is rejected for Agy because no isolated no-tools verifier mode has been measured.</>, <><IC>/loop</IC> 검증은 Agy에서 거부됩니다. 격리된 no-tools verifier mode가 실측되지 않았기 때문입니다.</>)}</li>
          <li>{t('Agy conversation files live under ~/.gemini/antigravity-cli because that is the storage path used by Antigravity CLI.', 'Agy 대화 파일은 Antigravity CLI가 사용하는 저장 경로인 ~/.gemini/antigravity-cli 아래에 있습니다.')}</li>
          <li>{t('Agy treats hook errors as fail-open. The ledger and acknowledgement let cokacdir detect a hook that never started (or did not complete) and discard its output, but cannot prove that Agy applied an otherwise valid hook response or undo model/tool side effects that already happened.', 'Agy는 훅 오류를 fail-open으로 처리합니다. ledger와 acknowledgement를 통해 훅이 시작되지 않았거나 완료되지 않은 경우를 감지해 출력을 폐기하지만, Agy가 형식상 유효한 훅 응답을 실제로 적용했는지는 증명할 수 없고 이미 발생한 모델·도구 부작용도 되돌릴 수 없습니다.')}</li>
          <li>{t('The system step is separate from user stdin, but Agy 1.1.1 still stores it as plaintext in the conversation database.', '시스템 단계는 사용자 stdin과 분리되지만 Agy 1.1.1은 이를 대화 데이터베이스에 평문으로 저장합니다.')}</li>
          <li>{t('Agy tool subprocesses inherit the hook file paths and token. Full permissions are intentional, so this transport is not a security boundary against same-user code.', 'Agy 도구 하위 프로세스도 훅 파일 경로와 토큰을 상속합니다. full 권한은 의도된 것이므로 이 전송 방식은 같은 사용자 권한의 코드에 대한 보안 경계가 아닙니다.')}</li>
        </UL>
      </SubSection>
    </div>
  )
}
