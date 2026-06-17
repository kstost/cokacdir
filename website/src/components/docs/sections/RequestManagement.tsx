import { SectionTitle, SubSection, P, IC, CommandTable, InfoBox } from '../DocComponents'
import { useLanguage } from '../../LanguageContext'

export default function RequestManagement() {
  const { t } = useLanguage()
  return (
    <div>
      <SectionTitle>{t('Request Management', '요청 관리')}</SectionTitle>
      <P>{t('Control AI requests and manage the message queue.', 'AI 요청을 제어하고 메시지 큐를 관리하세요.')}</P>

      <SubSection title={String(t('Commands', '명령어'))}>
        <CommandTable
          headers={[String(t('Command', '명령어')), String(t('Description', '설명'))]}
          rows={[
            ['/stop', String(t('Cancel current in-progress request (queue not affected)', '진행 중인 요청 취소 (큐는 영향 없음)'))],
            ['/stopall', String(t('Cancel current request and clear entire message queue', '현재 요청 취소 및 전체 메시지 큐 초기화'))],
            ['/stop <ID>', String(t('Remove a specific queued message by hex ID (case-insensitive)', '16진수 ID로 특정 대기 메시지 제거 (대소문자 무관)'))],
            ['/queue', String(t('Toggle queue mode on/off for current chat', '현재 채팅의 큐 모드 켜기/끄기'))],
          ]}
        />
      </SubSection>

      <SubSection title={String(t('Queue System', '큐 시스템'))}>
        <P>{t(
          'When the AI is busy processing a request, new messages are queued and processed in FIFO order.',
          'AI가 요청을 처리 중일 때 새 메시지는 큐에 추가되어 FIFO 순서로 처리됩니다.'
        )}</P>
        <ul className="list-disc list-inside space-y-2 text-zinc-400 my-4 ml-2">
          <li>{t(<>Maximum queue size: <strong className="text-zinc-300">20 messages</strong></>, <>최대 큐 크기: <strong className="text-zinc-300">20개 메시지</strong></>)}</li>
          <li>{t(<>Queued messages show an ID like <IC>A394FDA</IC> with options to cancel</>, <>대기 중인 메시지는 <IC>A394FDA</IC>와 같은 ID와 취소 옵션을 표시합니다</>)}</li>
          <li>{t('File uploads are captured at queue time and maintain context when processed', '파일 업로드는 큐 등록 시점에 캡처되며 처리 시 컨텍스트를 유지합니다')}</li>
          <li>{t('Full queue shows: "Queue full (max 20). Use /stopall to clear."', '큐가 가득 차면: "Queue full (max 20). Use /stopall to clear." 표시')}</li>
        </ul>
      </SubSection>

      <SubSection title={String(t('Queue Mode', '큐 모드'))}>
        <CommandTable
          headers={[String(t('Mode', '모드')), String(t('Behavior', '동작'))]}
          rows={[
            [String(t('ON (default)', 'ON (기본값)')), String(t('Messages are queued while AI is busy', 'AI가 처리 중일 때 메시지를 큐에 추가'))],
            ['OFF', String(t('Messages are rejected with "AI request in progress"', '"AI request in progress" 메시지와 함께 거부'))],
          ]}
        />
        <P>{t(<>Toggle with <IC>/queue</IC>.</>, <><IC>/queue</IC>로 전환합니다.</>)}</P>
      </SubSection>

      <SubSection title={String(t('Interaction Behavior', '상호작용 동작'))}>
        <CommandTable
          headers={[String(t('Scenario', '상황')), '/stop', '/stopall']}
          rows={[
            [String(t('AI is processing', 'AI 처리 중')), String(t('Cancels current request', '현재 요청 취소')), String(t('Cancels current request + clears queue', '현재 요청 취소 + 큐 초기화'))],
            [String(t('Messages in queue', '큐에 메시지 있음')), String(t('No effect on queue', '큐에 영향 없음')), String(t('Clears entire queue', '전체 큐 초기화'))],
            [String(t('Specific message', '특정 메시지')), String(t('/stop <ID> removes it', '/stop <ID>로 제거')), String(t('Removes all queued messages', '모든 대기 메시지 제거'))],
          ]}
        />
      </SubSection>

      <SubSection title={String(t('Loop — Self-Verification Loop', '루프 — 자가 검증 반복'))}>
        <P>{t(
          <>The <IC>/loop</IC> command keeps running the same task until the bot itself decides it is fully and correctly completed. After every response, the bot runs a provider-specific verification step and asks the AI to judge whether the work is done. If not, it re-injects the remaining work as the next prompt and tries again.</>,
          <><IC>/loop</IC> 명령은 봇이 작업이 완전하고 올바르게 끝났다고 스스로 판단할 때까지 같은 작업을 반복 실행합니다. 매 응답 직후 봇은 프로바이더별 검증 단계를 실행하여 AI에게 작업 완료 여부를 평가하도록 요청합니다. 끝나지 않았다면 남은 작업을 다음 프롬프트로 재주입하고 다시 시도합니다.</>
        )}</P>
        <P>{t(
          <>Useful for tasks where one shot is rarely enough — multi-step refactors, "keep trying until tests pass", "fix everything the linter reports", and similar.</>,
          <>한 번에 끝나기 어려운 작업에 유용합니다 — 다단계 리팩터링, "테스트가 통과할 때까지 계속 시도", "린터가 보고한 모든 것을 수정" 같은 경우입니다.</>
        )}</P>
        <h3 className="text-lg font-semibold text-white mt-6 mb-3">
          {t('Usage', '사용법')}
        </h3>
        <CommandTable
          headers={[String(t('Command', '명령어')), String(t('Description', '설명'))]}
          rows={[
            ['/loop <request>', String(t('Repeat up to 5 times (default)', '최대 5회 반복 (기본값)'))],
            ['/loop <N> <request>', String(t('Repeat up to N times', '최대 N회 반복'))],
            ['/loop 0 <request>', String(t('Repeat with no upper bound — use with care', '상한 없이 반복 — 주의해서 사용'))],
          ]}
        />
        <P>{t(
          <>Examples: <IC>/loop fix all clippy warnings</IC>, <IC>/loop 10 add unit tests until coverage is above 90%</IC>, <IC>/loop 0 keep trying until the build passes</IC>.</>,
          <>예: <IC>/loop fix all clippy warnings</IC>, <IC>/loop 10 add unit tests until coverage is above 90%</IC>, <IC>/loop 0 keep trying until the build passes</IC>.</>
        )}</P>
        <h3 className="text-lg font-semibold text-white mt-6 mb-3">
          {t('Requirements', '요구 사항')}
        </h3>
        <ul className="list-disc list-inside space-y-2 text-zinc-400 my-4 ml-2">
          <li>{t(
            <><strong className="text-zinc-300">Claude, Codex, or OpenCode model.</strong> Each provider uses its own isolation mechanism for verification: Claude uses the native <IC>--fork-session</IC>, Codex runs an independent <IC>codex exec --ephemeral</IC> with a transcript synthesized from the full-fidelity archive (original rollout file stays byte-identical), and OpenCode uses the native <IC>opencode run --session &lt;id&gt; --fork --agent plan</IC>. Agy is still rejected.</>,
            <><strong className="text-zinc-300">Claude, Codex, OpenCode 모델 지원.</strong> 각 프로바이더는 검증에 자체 격리 방식을 사용합니다: Claude는 네이티브 <IC>--fork-session</IC>, Codex는 full-fidelity 아카이브에서 트랜스크립트를 합성해 독립 <IC>codex exec --ephemeral</IC>을 실행(원본 rollout 파일 바이트 단위로 불변), OpenCode는 네이티브 <IC>opencode run --session &lt;id&gt; --fork --agent plan</IC>을 사용합니다. Agy는 거부됩니다.</>
          )}</li>
          <li>{t(
            <><strong className="text-zinc-300">One loop per chat at a time.</strong> If a loop is already running, a new <IC>/loop</IC> is rejected — cancel with <IC>/stop</IC> first.</>,
            <><strong className="text-zinc-300">채팅당 동시 한 개의 루프만 가능.</strong> 이미 루프가 진행 중이면 새 <IC>/loop</IC>는 거부됩니다 — 먼저 <IC>/stop</IC>으로 취소하세요.</>
          )}</li>
        </ul>
        <h3 className="text-lg font-semibold text-white mt-6 mb-3">
          {t('What You\'ll See', '표시되는 메시지')}
        </h3>
        <CommandTable
          headers={[String(t('Message', '메시지')), String(t('Meaning', '의미'))]}
          rows={[
            ['🔄 Loop started (max N iterations)', String(t('Loop has begun', '루프가 시작됨'))],
            ['🔄 Loop started (unlimited)', String(t('Started in /loop 0 mode', '/loop 0 모드로 시작'))],
            ['🔍 Verifying... (animated 🔍/🔎 spinner)', String(t('Bot is running the verification step to judge completeness', '봇이 완료 여부를 판단하기 위해 검증 단계를 실행하는 중'))],
            ['🔄 Loop iteration K/N + feedback', String(t('Verification said incomplete; re-injecting feedback', '검증 결과 미완료; 피드백을 재주입'))],
            ['✅ Loop complete — task verified as done.', String(t('Verification said complete; loop ends', '검증 결과 완료; 루프 종료'))],
            ['⚠️ Loop limit reached. Remaining issue: ...', String(t('Hit the iteration cap before completion', '완료 전에 반복 한도 도달'))],
            ['⚠️ Loop verification failed: ...', String(t('The verify step itself errored; loop aborted', '검증 단계 자체에서 오류; 루프 중단'))],
          ]}
        />
        <h3 className="text-lg font-semibold text-white mt-6 mb-3">
          {t('Stopping a Loop', '루프 중단하기')}
        </h3>
        <ul className="list-disc list-inside space-y-2 text-zinc-400 my-4 ml-2">
          <li>{t(<><IC>/stop</IC> — cancels the current iteration and the loop. The verifier will not re-inject after stop.</>, <><IC>/stop</IC> — 현재 반복과 루프를 모두 취소합니다. 중단 후 검증기는 재주입하지 않습니다.</>)}</li>
          <li>{t(<><IC>/stopall</IC> — same, plus clears any queued messages.</>, <><IC>/stopall</IC> — 위와 같으며 추가로 모든 큐 메시지를 비웁니다.</>)}</li>
          <li>{t(<><IC>/clear</IC> — also clears loop state along with the session.</>, <><IC>/clear</IC> — 세션과 함께 루프 상태도 초기화합니다.</>)}</li>
        </ul>
        <h3 className="text-lg font-semibold text-white mt-6 mb-3">
          {t('Tips', '팁')}
        </h3>
        <ul className="list-disc list-inside space-y-2 text-zinc-400 my-4 ml-2">
          <li>{t(
            <><IC>/loop 0</IC> (unlimited) is powerful but has no built-in safety net. Pair it with a clear stopping criterion in the request itself (e.g., "until the test command exits 0").</>,
            <><IC>/loop 0</IC>(무한)은 강력하지만 안전 장치가 내장되어 있지 않습니다. 요청 자체에 명확한 종료 조건을 함께 명시하세요(예: "테스트 명령이 0으로 종료될 때까지").</>
          )}</li>
          <li>{t(
            <>Each iteration is a real AI turn — token cost scales with iteration count. The verification step is also an AI call (single turn, no tools), so each loop iteration costs roughly <strong className="text-zinc-300">1 task turn + 1 verify turn</strong>.</>,
            <>매 반복은 실제 AI 턴입니다 — 토큰 비용은 반복 횟수에 비례합니다. 검증 단계도 AI 호출(단일 턴, 도구 없음)이므로 매 루프 반복은 대략 <strong className="text-zinc-300">1 작업 턴 + 1 검증 턴</strong>의 비용이 듭니다.</>
          )}</li>
        </ul>
      </SubSection>

      <SubSection title={String(t('End Hook — Notification When Processing Completes', '엔드 훅 — 처리 완료 알림'))}>
        <P>{t(
          <>The end hook is a custom message the bot sends as a separate chat message every time an AI request finishes. Works on Telegram, Discord, and Slack — useful as a ping when you walk away from a long-running task.</>,
          <>엔드 훅은 AI 요청이 끝날 때마다 봇이 별도의 채팅 메시지로 보내는 사용자 정의 알림입니다. Telegram, Discord, Slack 모두에서 동작하며, 오래 걸리는 작업을 띄워두고 자리를 비울 때 완료 시점을 알려주는 용도로 유용합니다.</>
        )}</P>
        <CommandTable
          headers={[String(t('Command', '명령어')), String(t('Description', '설명'))]}
          rows={[
            ['/setendhook <message>', String(t('Set the end hook message for this chat', '이 채팅의 엔드 훅 메시지를 설정'))],
            ['/setendhook', String(t('Show the currently configured end hook (or report none)', '현재 설정된 엔드 훅을 표시 (없으면 그렇게 안내)'))],
            ['/setendhook_clear', String(t('Remove the end hook for this chat', '이 채팅의 엔드 훅 제거'))],
          ]}
        />
        <P>{t(
          <>Example: <IC>/setendhook ✅ Done</IC> — after every successful completion, the bot will send <IC>✅ Done</IC> as a follow-up message right after the AI's response.</>,
          <>예: <IC>/setendhook ✅ Done</IC> — 매번 정상적으로 완료될 때마다 봇이 AI 응답 직후에 <IC>✅ Done</IC>을 후속 메시지로 보냅니다.</>
        )}</P>
        <h3 className="text-lg font-semibold text-white mt-6 mb-3">
          {t('When it fires', '발송 시점')}
        </h3>
        <ul className="list-disc list-inside space-y-2 text-zinc-400 my-4 ml-2">
          <li>{t('After every normal AI response completes', '일반 AI 응답이 완료된 후')}</li>
          <li>{t('After shell command execution finishes', '셸 명령 실행이 끝난 후')}</li>
          <li>{t('After scheduled tasks complete', '스케줄된 작업이 완료된 후')}</li>
          <li>{t('After bot-to-bot messages complete', '봇 간 메시지가 완료된 후')}</li>
        </ul>
        <h3 className="text-lg font-semibold text-white mt-6 mb-3">
          {t('When it does NOT fire', '발송되지 않는 경우')}
        </h3>
        <ul className="list-disc list-inside space-y-2 text-zinc-400 my-4 ml-2">
          <li>{t(<>When the request is cancelled with <IC>/stop</IC> or <IC>/stopall</IC></>, <>요청이 <IC>/stop</IC> 또는 <IC>/stopall</IC>으로 취소된 경우</>)}</li>
          <li>{t('When no end hook is configured for the chat', '해당 채팅에 엔드 훅이 설정되어 있지 않은 경우')}</li>
        </ul>
        <P>{t(
          <>The end hook is stored <strong>per chat</strong>, so different chats can use different markers. Combine with mobile push notifications and the bot becomes a long-task pager.</>,
          <>엔드 훅은 <strong>채팅별</strong>로 저장되므로 채팅마다 다른 표식을 쓸 수 있습니다. 모바일 푸시 알림과 결합하면 봇이 장시간 작업의 호출기 역할을 해냅니다.</>
        )}</P>
      </SubSection>

      <SubSection title={String(t('Codex image_gen — Auto-Delivered Images', 'Codex image_gen — 이미지 자동 전달'))}>
        <P>{t(
          <>When you use <strong>Codex CLI</strong> as the AI provider and the model invokes its built-in <IC>image_gen</IC> tool, the generated images are written to <IC>~/.codex/generated_images/&lt;session_id&gt;/</IC> without surfacing any tool event. cokacdir scans that directory at the end of each turn and automatically delivers any new images the model produced — you do not need to ask the model to call <IC>--sendfile</IC> for them.</>,
          <>AI 제공자로 <strong>Codex CLI</strong>를 사용하고 모델이 내장 <IC>image_gen</IC> 도구를 호출하면, 생성된 이미지는 도구 이벤트 없이 <IC>~/.codex/generated_images/&lt;session_id&gt;/</IC>에 저장됩니다. cokacdir은 매 턴 종료 시 이 디렉터리를 스캔하여 모델이 생성한 새 이미지를 자동으로 전달합니다 — 모델이 <IC>--sendfile</IC>을 호출하도록 요청할 필요가 없습니다.</>
        )}</P>
        <ul className="list-disc list-inside space-y-1.5 text-zinc-400 my-4 ml-2">
          <li>{t('Only files created during the current turn are sent; pre-existing files in the session directory are ignored.', '현재 턴에 생성된 파일만 전송됩니다. 세션 디렉터리에 미리 존재하던 파일은 무시됩니다.')}</li>
          <li>{t(<>Files the model already delivered via <IC>--sendfile</IC> in the same turn are skipped to avoid duplicates.</>, <>같은 턴에 모델이 이미 <IC>--sendfile</IC>로 전달한 파일은 중복 방지를 위해 건너뜁니다.</>)}</li>
          <li>{t('Supported extensions: png, jpg, jpeg, webp, gif, bmp.', '지원 확장자: png, jpg, jpeg, webp, gif, bmp.')}</li>
        </ul>
        <InfoBox type="info">
          {t(
            'This auto-delivery is Codex-specific — Claude Code, Agy, and OpenCode do not write to that directory and are unaffected.',
            '이 자동 전달은 Codex 전용입니다 — Claude Code, Agy, OpenCode는 해당 디렉터리에 쓰지 않으며 영향을 받지 않습니다.'
          )}
        </InfoBox>
      </SubSection>
    </div>
  )
}
