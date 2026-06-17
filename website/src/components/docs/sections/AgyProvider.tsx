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
          <>Agy is run in print mode. The prompt is written through stdin, and cokacdir always passes an explicit empty string to <IC>--print</IC>.</>,
          <>Agy는 print mode로 실행됩니다. 프롬프트는 stdin으로 전달하고, cokacdir는 <IC>--print</IC>에 항상 명시적인 빈 문자열을 넘깁니다.</>
        )}</P>
        <CodeBlock code={'agy --print "" --print-timeout <duration> --log-file <temp-log> --dangerously-skip-permissions'} />
        <P>{t(
          <>For resumed sessions, cokacdir adds <IC>--conversation &lt;session_id&gt;</IC>. The bare form <IC>agy --print</IC> is intentionally avoided because measured runs showed it can consume unintended context and produce unrelated output.</>,
          <>세션 재개 시에는 <IC>--conversation &lt;session_id&gt;</IC>를 추가합니다. 실측 결과 <IC>agy --print</IC>처럼 값 없는 형태는 의도하지 않은 컨텍스트를 소비해 무관한 출력을 만들 수 있어 의도적으로 피합니다.</>
        )}</P>
      </SubSection>

      <SubSection title={String(t('Stdout Contract', 'stdout 규약'))}>
        <P>{t(
          <>Agy print mode emits plain stdout. It does not emit structured tool-use events like Claude or Codex JSONL streams.</>,
          <>Agy print mode는 평문 stdout을 출력합니다. Claude나 Codex의 JSONL 스트림처럼 구조화된 도구 사용 이벤트를 내보내지 않습니다.</>
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
        </UL>
      </SubSection>
    </div>
  )
}
