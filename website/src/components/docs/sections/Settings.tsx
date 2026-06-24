import { Link } from 'react-router-dom'
import { SectionTitle, SubSection, P, IC, InfoBox, CommandTable, CodeBlock } from '../DocComponents'
import { useLanguage } from '../../LanguageContext'

export default function Settings() {
  const { t } = useLanguage()
  return (
    <div>
      <SectionTitle>{t('Settings', '설정')}</SectionTitle>
      <P>{t('Configure various settings for your chat environment.', '채팅 환경의 다양한 설정을 구성하세요.')}</P>

      <SubSection title={String(t('Command Reference', '명령어 참조'))}>
        <CommandTable
          headers={[String(t('Command', '명령어')), String(t('Description', '설명'))]}
          rows={[
            ['/silent', String(t('View/set output mode (compact/final/verbose)', '출력 모드 보기/설정 (compact/final/verbose)'))],
            ['/debug', String(t('Enable/disable debug logging', '디버그 로깅 활성화/비활성화'))],
            ['/effort <level>', String(t('Set Claude/Codex effort level', 'Claude/Codex effort 수준 설정'))],
            ['/fast', String(t('Toggle Codex fast service tier', 'Codex fast service tier 전환'))],
            ['/stt_model <model>', String(t('Set speech recognition model', '음성 인식 모델 설정'))],
            ['/setpollingtime <ms>', String(t('Set API polling interval', 'API 폴링 간격 설정'))],
            ['/envvars', String(t('Show all environment variables (owner only)', '모든 환경변수 표시 (소유자 전용)'))],
            ['/help', String(t('Display full command reference', '전체 명령어 참조 표시'))],
          ]}
        />
      </SubSection>

      <SubSection title="/silent">
        <P>{t(<>Configures output verbosity for the current chat. Default: <strong className="text-zinc-300">compact</strong>.</>, <>현재 채팅의 출력 상세도를 설정합니다. 기본값: <strong className="text-zinc-300">compact</strong>입니다.</>)}</P>
        <CodeBlock code={`/silent
/silent status
/silent compact
/silent final
/silent verbose`} />
        <ul className="list-disc list-inside space-y-1.5 text-zinc-400 my-4 ml-2">
          <li>{t(<><strong className="text-zinc-300">compact:</strong> hides tool calls/results while keeping normal AI text and progress visible</>, <><strong className="text-zinc-300">compact:</strong> 도구 호출/결과는 숨기고 일반 AI 텍스트와 진행 표시는 유지합니다</>)}</li>
          <li>{t(<><strong className="text-zinc-300">final:</strong> shows the animated clock/<IC>Processing</IC> placeholder, then replaces it with the final response</>, <><strong className="text-zinc-300">final:</strong> 시계/<IC>Processing</IC> 애니메이션 placeholder를 먼저 보여주고 최종 응답으로 교체합니다</>)}</li>
          <li>{t(<><strong className="text-zinc-300">verbose:</strong> shows full tool call details, summaries, results, and errors</>, <><strong className="text-zinc-300">verbose:</strong> 도구 호출 상세, 요약, 결과, 오류를 모두 표시합니다</>)}</li>
        </ul>
      </SubSection>

      <SubSection title="/debug">
        <P>{t(<>Enables or disables debug logging. Default: <strong className="text-zinc-300">OFF</strong>.</>, <>디버그 로깅을 활성화/비활성화합니다. 기본값: <strong className="text-zinc-300">OFF</strong>.</>)}</P>
        <P>{t('When enabled, logs details for:', '활성화 시 다음 항목의 세부 로그를 기록합니다:')}</P>
        <ul className="list-disc list-inside space-y-1.5 text-zinc-400 my-4 ml-2">
          <li>{t('Messenger API operations (Telegram, Discord, Slack)', '메신저 API 작업 (Telegram, Discord, Slack)')}</li>
          <li>{t('AI service calls', 'AI 서비스 호출')}</li>
          <li>{t('Cron scheduler', '크론 스케줄러')}</li>
        </ul>
        <InfoBox type="info">
          {t(
            'The preference is stored per bot, but debug logging is process-wide while the bot server is running. If any bot in the same process has debug enabled, shared debug logs remain on.',
            '설정값은 봇별로 저장되지만, 봇 서버가 실행 중일 때 디버그 로깅은 프로세스 전체에 적용됩니다. 같은 프로세스의 어떤 봇이라도 디버그가 켜져 있으면 공유 디버그 로그는 계속 켜져 있습니다.'
          )}
        </InfoBox>
      </SubSection>

      <SubSection title="/effort">
        <P>{t(
          <>Sets the effort level for the active <IC>Claude</IC> or <IC>Codex</IC> provider in the current chat.</>,
          <>현재 채팅에서 활성화된 <IC>Claude</IC> 또는 <IC>Codex</IC> provider의 effort 수준을 설정합니다.</>
        )}</P>
        <ul className="list-disc list-inside space-y-1.5 text-zinc-400 my-4 ml-2">
          <li>{t(<>Claude: <IC>low</IC>, <IC>medium</IC>, <IC>high</IC>, <IC>xhigh</IC>, <IC>max</IC></>, <>Claude: <IC>low</IC>, <IC>medium</IC>, <IC>high</IC>, <IC>xhigh</IC>, <IC>max</IC></>)}</li>
          <li>{t(<>Codex: <IC>minimal</IC>, <IC>low</IC>, <IC>medium</IC>, <IC>high</IC>, <IC>xhigh</IC></>, <>Codex: <IC>minimal</IC>, <IC>low</IC>, <IC>medium</IC>, <IC>high</IC>, <IC>xhigh</IC></>)}</li>
        </ul>
        <P>{t(
          <><IC>/effort reset</IC> clears the override for the current provider.</>,
          <><IC>/effort reset</IC>은 현재 provider의 override를 해제합니다.</>
        )}</P>
      </SubSection>

      <SubSection title="/fast">
        <P>{t(
          <>Toggles Codex Fast mode for the current chat. When enabled, Codex receives <IC>-c service_tier="fast"</IC>.</>,
          <>현재 채팅의 Codex Fast mode를 전환합니다. 활성화되면 Codex에 <IC>-c service_tier="fast"</IC>가 전달됩니다.</>
        )}</P>
        <P>{t(
          <><IC>/fast off</IC> removes the per-chat override and Codex uses its default/configured service tier.</>,
          <><IC>/fast off</IC>는 채팅별 override를 제거하며 Codex는 기본/설정된 service tier를 사용합니다.</>
        )}</P>
      </SubSection>

      <SubSection title="/stt_model">
        <P>{t(
          <>Sets the transcriptor speech recognition model for the current chat. Bare model names are passed as <IC>--model-name</IC> and override an inherited <IC>TRANSCRIPTOR_MODEL</IC> value for that run; <IC>path:&lt;model_path&gt;</IC> is passed as <IC>--model</IC>.</>,
          <>현재 채팅의 transcriptor 음성 인식 모델을 설정합니다. 일반 모델명은 <IC>--model-name</IC>으로 전달되고 해당 실행에서 상속된 <IC>TRANSCRIPTOR_MODEL</IC> 값을 무시하며, <IC>path:&lt;model_path&gt;</IC>는 <IC>--model</IC>로 전달됩니다.</>
        )}</P>
        <CodeBlock code={`/stt_model
/stt_model small
/stt_model large-v3-turbo
/stt_model path:/absolute/model.bin
/stt_model reset`} />
        <P>{t(
          'If the selected model is not cached yet, Telegram STT progress messages show the model download before recognition continues.',
          '선택한 모델이 아직 캐시되어 있지 않으면 Telegram STT 진행 메시지가 모델 다운로드를 먼저 표시한 뒤 인식을 이어갑니다.'
        )}</P>
      </SubSection>

      <SubSection title="/setpollingtime">
        <P>{t(
          'Sets the API polling interval in milliseconds. This controls how frequently streaming responses and shell command output are updated.',
          'API 폴링 간격을 밀리초 단위로 설정합니다. 스트리밍 응답과 셸 명령어 출력이 업데이트되는 빈도를 제어합니다.'
        )}</P>
        <ul className="list-disc list-inside space-y-1.5 text-zinc-400 my-4 ml-2">
          <li>{t(<>Minimum: <strong className="text-zinc-300">2500ms</strong></>, <>최소: <strong className="text-zinc-300">2500ms</strong></>)}</li>
          <li>{t(<>Recommended: <strong className="text-zinc-300">3000ms or higher</strong></>, <>권장: <strong className="text-zinc-300">3000ms 이상</strong></>)}</li>
        </ul>
      </SubSection>

      <SubSection title="/envvars">
        <P>{t(
          <>Prints every environment variable currently visible to the bot process, sorted alphabetically. <strong>Bot owner only.</strong></>,
          <>봇 프로세스에 현재 보이는 모든 환경변수를 알파벳순으로 출력합니다. <strong>봇 소유자 전용입니다.</strong></>
        )}</P>
        <P>{t(
          <>Useful for verifying that <IC>~/.cokacdir/.env.json</IC> loaded correctly, or checking whether a <IC>COKAC_*</IC> override is active.</>,
          <><IC>~/.cokacdir/.env.json</IC>이 올바르게 로드되었는지, 또는 <IC>COKAC_*</IC> 재지정이 활성 상태인지 확인할 때 유용합니다.</>
        )}</P>
        <InfoBox type="warning">
          {t(
            <><strong>⚠ Security warning:</strong> <IC>/envvars</IC> exposes <strong>all</strong> environment variables with no redaction — including API keys, tokens, and credentials. Chat platforms (Telegram, Discord, Slack) store message history on their servers, so anything printed by this command is persisted until you delete the messages. Use it only for diagnostics, clear the response afterward, and <strong>always use it in a 1:1 chat (DM)</strong>. Group chats are rejected for this command.</>,
            <><strong>⚠ 보안 경고:</strong> <IC>/envvars</IC>는 API 키, 토큰, 자격 증명을 포함한 <strong>모든</strong> 환경변수를 마스킹 없이 노출합니다. 채팅 플랫폼(Telegram, Discord, Slack)은 메시지 기록을 서버에 저장하므로, 이 명령으로 출력된 내용은 메시지를 삭제할 때까지 남아 있습니다. 진단 용도로만 사용하고, 사용 후에는 응답을 삭제하며, <strong>항상 1:1 채팅(DM)에서만 사용하세요</strong>. 이 명령은 그룹 채팅에서 거부됩니다.</>
          )}
        </InfoBox>
        <P>{t(
          <>See <Link to="/docs/env-vars" className="text-accent-cyan hover:underline">Environment Variables</Link> for the full list of variables cokacdir reads and for the <IC>~/.cokacdir/.env.json</IC> auto-loader.</>,
          <>cokacdir가 읽는 변수 전체 목록과 <IC>~/.cokacdir/.env.json</IC> 자동 로더에 대해서는 <Link to="/docs/env-vars" className="text-accent-cyan hover:underline">환경변수</Link> 페이지를 참조하세요.</>
        )}</P>
      </SubSection>

      <SubSection title="/help">
        <P>{t(
          'Displays the full command reference with all available commands and their descriptions.',
          '사용 가능한 모든 명령어와 설명이 포함된 전체 명령어 참조를 표시합니다.'
        )}</P>
      </SubSection>
    </div>
  )
}
