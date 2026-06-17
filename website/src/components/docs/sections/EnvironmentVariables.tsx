import { SectionTitle, SubSection, P, IC, InfoBox, CodeBlock, UL, CommandTable } from '../DocComponents'
import { useLanguage } from '../../LanguageContext'

export default function EnvironmentVariables() {
  const { t } = useLanguage()
  return (
    <div>
      <SectionTitle>{t('Environment Variables', '환경변수')}</SectionTitle>
      <P>{t(
        'cokacdir reads a number of environment variables at startup to override binary paths, tune internal limits, and toggle debug logging. This page describes every environment variable the program consults, how to set them, and how to inspect their current values from within a running bot.',
        'cokacdir는 시작 시 바이너리 경로 재지정, 내부 한계값 조정, 디버그 로깅 전환을 위해 여러 환경변수를 읽습니다. 이 페이지는 프로그램이 참조하는 모든 환경변수, 설정 방법, 그리고 실행 중인 봇에서 현재 값을 확인하는 방법을 설명합니다.'
      )}</P>

      <SubSection title={String(t('Where to Set Environment Variables', '환경변수를 설정하는 위치'))}>
        <h3 className="text-lg font-semibold text-white mt-6 mb-3">
          {t('1. ~/.cokacdir/.env.json (recommended)', '1. ~/.cokacdir/.env.json (권장)')}
        </h3>
        <P>{t(
          <>On startup, cokacdir reads <IC>~/.cokacdir/.env.json</IC> and injects every key/value pair from that file into the process environment. This is the most convenient place to store configuration because it persists across sessions without touching your shell profile.</>,
          <>시작 시 cokacdir는 <IC>~/.cokacdir/.env.json</IC>을 읽고 파일의 모든 키/값 쌍을 프로세스 환경에 주입합니다. 셸 프로필을 건드리지 않고도 세션 간 설정이 유지되므로 가장 편리한 설정 저장 위치입니다.</>
        )}</P>
        <CodeBlock code={`{
  "COKAC_CLAUDE_PATH": "/home/alice/.local/bin/claude",
  "COKAC_CODEX_PATH": "/opt/codex/codex",
  "COKAC_FILE_ATTACH_THRESHOLD": "16384",
  "COKACDIR_DEBUG": "1"
}`} />
        <P>{t(
          <>The file must contain a <strong>JSON object</strong> at the root. Each key becomes an environment variable name, and its value becomes the value of that variable. Supported value types are <strong>string</strong>, <strong>number</strong>, and <strong>boolean</strong>. Objects, arrays, and <IC>null</IC> values are skipped with a warning printed to stderr.</>,
          <>파일은 루트에 <strong>JSON 객체</strong>를 포함해야 합니다. 각 키는 환경변수 이름이 되고, 그 값이 해당 변수의 값이 됩니다. 지원하는 값 타입은 <strong>string</strong>, <strong>number</strong>, <strong>boolean</strong>입니다. 객체, 배열, <IC>null</IC> 값은 stderr에 경고를 출력하고 건너뜁니다.</>
        )}</P>
        <InfoBox type="warning">
          {t(
            <><strong>Values in <IC>.env.json</IC> take priority over the existing environment.</strong> If you already have <IC>COKAC_CLAUDE_PATH</IC> exported in your shell and also set it in <IC>.env.json</IC>, the <IC>.env.json</IC> value wins. Use <IC>.env.json</IC> as the single source of truth rather than mixing with shell exports to avoid confusion.</>,
            <><strong><IC>.env.json</IC>의 값은 기존 환경변수보다 우선합니다.</strong> 셸에서 <IC>COKAC_CLAUDE_PATH</IC>를 export했더라도 <IC>.env.json</IC>에 같은 키가 있으면 <IC>.env.json</IC>의 값이 적용됩니다. 혼란을 피하려면 셸 export와 섞지 말고 <IC>.env.json</IC>을 단일 정보 출처로 사용하세요.</>
          )}
        </InfoBox>
        <P>{t(
          'If the file does not exist, cokacdir silently proceeds with whatever is already in the process environment. If the file exists but contains invalid JSON (or a non-object root like a JSON array), a warning is printed and the file is ignored — startup continues normally.',
          '파일이 존재하지 않으면 cokacdir는 기존 프로세스 환경을 그대로 사용하며 조용히 진행합니다. 파일이 존재하지만 JSON이 잘못된 경우(또는 JSON 배열처럼 객체가 아닌 루트인 경우) 경고가 출력되고 파일은 무시되지만, 시작은 정상적으로 계속됩니다.'
        )}</P>
        <InfoBox type="warning">
          {t(
            <>
              <strong>⚠ Boolean and number values are serialized to strings literally.</strong> If you write <IC>{`{"COKACDIR_DEBUG": true}`}</IC>, cokacdir sets the environment variable to the literal string <IC>"true"</IC> — not <IC>"1"</IC>. Since <IC>COKACDIR_DEBUG</IC> only enables debug when its value equals <IC>"1"</IC>, writing <IC>true</IC> will <em>not</em> enable debug. Use the string <IC>"1"</IC> or the number <IC>1</IC> instead. Always check each variable's documented format below rather than assuming truthy-coercion.
            </>,
            <>
              <strong>⚠ Boolean과 number 값은 문자열로 그대로 직렬화됩니다.</strong> <IC>{`{"COKACDIR_DEBUG": true}`}</IC>라고 쓰면 cokacdir는 환경변수를 <IC>"1"</IC>이 아닌 문자열 <IC>"true"</IC>로 설정합니다. <IC>COKACDIR_DEBUG</IC>는 값이 정확히 <IC>"1"</IC>일 때만 디버그를 활성화하므로, <IC>true</IC>로 쓰면 디버그가 <em>켜지지 않습니다</em>. 대신 문자열 <IC>"1"</IC>이나 숫자 <IC>1</IC>을 사용하세요. 각 변수의 형식은 아래 문서에서 반드시 확인하고, truthy-coercion을 가정하지 마세요.
            </>
          )}
        </InfoBox>

        <h3 className="text-lg font-semibold text-white mt-6 mb-3">
          {t('2. Shell exports', '2. 셸 export')}
        </h3>
        <P>{t(
          <>You can also export variables the usual way before launching <IC>cokacdir</IC> or <IC>cokacctl</IC>:</>,
          <><IC>cokacdir</IC> 또는 <IC>cokacctl</IC>을 실행하기 전에 일반적인 방법으로 변수를 export할 수도 있습니다:</>
        )}</P>
        <CodeBlock code={`export COKAC_CLAUDE_PATH=/home/alice/.local/bin/claude
cokacctl`} />
        <P>{t(
          <>This works, but any keys that also appear in <IC>~/.cokacdir/.env.json</IC> will be overwritten when the program starts.</>,
          <>동작은 하지만, <IC>~/.cokacdir/.env.json</IC>에 같은 키가 있으면 프로그램 시작 시 덮어써집니다.</>
        )}</P>
      </SubSection>

      <SubSection title={String(t('Environment Variable Reference', '환경변수 참조'))}>
        <CommandTable
          headers={[
            String(t('Variable', '변수')),
            String(t('Purpose', '용도')),
            String(t('Default', '기본값')),
          ]}
          rows={[
            [
              <IC key="1">COKAC_CLAUDE_PATH</IC>,
              String(t('Override path to Claude CLI binary', 'Claude CLI 바이너리 경로 재지정')),
              String(t('auto-resolved', '자동 탐색')),
            ],
            [
              <IC key="2">COKAC_CODEX_PATH</IC>,
              String(t('Override path to Codex CLI binary', 'Codex CLI 바이너리 경로 재지정')),
              String(t('auto-resolved', '자동 탐색')),
            ],
            [
              <IC key="3">COKAC_AGY_PATH</IC>,
              String(t('Override path to Antigravity CLI agy binary', 'Antigravity CLI agy 바이너리 경로 재지정')),
              String(t('auto-resolved', '자동 탐색')),
            ],
            [
              <IC key="4">COKAC_OPENCODE_PATH</IC>,
              String(t('Override path to Opencode CLI binary (Unix only)', 'Opencode CLI 바이너리 경로 재지정 (Unix 전용)')),
              String(t('auto-resolved', '자동 탐색')),
            ],
            [
              <IC key="5">COKAC_FILE_ATTACH_THRESHOLD</IC>,
              String(t('Byte threshold for switching to .txt file attachment', '.txt 파일 첨부로 전환할 바이트 임계값')),
              '8192',
            ],
            [
              <IC key="6">COKACDIR_DEBUG</IC>,
              String(t('Set to "1" to enable debug logging at startup', '"1"로 설정 시 시작 시점부터 디버그 로깅 활성화')),
              String(t('off', '꺼짐')),
            ],
          ]}
        />

        <h3 className="text-lg font-semibold text-white mt-6 mb-3"><IC>COKAC_CLAUDE_PATH</IC></h3>
        <P>{t(
          <>Override the path to the Claude CLI binary. Normally cokacdir resolves Claude automatically with <IC>which claude</IC> (falling back to <IC>bash -lc "which claude"</IC> for non-interactive SSH sessions, and <IC>SearchPathW</IC> on Windows). Set this variable if you want to pin a specific installation, or if automatic resolution fails in your environment.</>,
          <>Claude CLI 바이너리 경로를 재지정합니다. 일반적으로 cokacdir는 <IC>which claude</IC>로 자동 탐색하고(비대화형 SSH 세션에서는 <IC>bash -lc "which claude"</IC>, Windows에서는 <IC>SearchPathW</IC>로 폴백) 작동합니다. 특정 설치본을 고정하고 싶거나 자동 탐색이 실패하는 환경이라면 이 변수를 설정하세요.</>
        )}</P>
        <UL>
          <li>{t(<><strong>Type:</strong> absolute path to an existing executable</>, <><strong>타입:</strong> 존재하는 실행 파일의 절대 경로</>)}</li>
          <li>{t(<><strong>Default:</strong> not set (automatic resolution)</>, <><strong>기본값:</strong> 설정되지 않음 (자동 탐색)</>)}</li>
          <li>{t(
            <><strong>Behavior:</strong> If the value is empty or points to a non-existent file, cokacdir falls through to the normal resolution logic rather than failing.</>,
            <><strong>동작:</strong> 값이 비어 있거나 존재하지 않는 파일을 가리키면, cokacdir는 실패하지 않고 일반 탐색 로직으로 폴백합니다.</>
          )}</li>
        </UL>
        <CodeBlock code="COKAC_CLAUDE_PATH=/home/alice/.local/bin/claude" />

        <h3 className="text-lg font-semibold text-white mt-6 mb-3"><IC>COKAC_CODEX_PATH</IC></h3>
        <P>{t(
          <>Override the path to the Codex CLI binary. Same semantics as <IC>COKAC_CLAUDE_PATH</IC> but for Codex. On Windows, the fallback resolver prefers <IC>.cmd</IC> (npm batch wrapper) over <IC>.exe</IC>.</>,
          <>Codex CLI 바이너리 경로를 재지정합니다. <IC>COKAC_CLAUDE_PATH</IC>와 동일한 의미이지만 Codex용입니다. Windows에서 폴백 탐색기는 <IC>.exe</IC>보다 <IC>.cmd</IC>(npm 배치 래퍼)를 우선합니다.</>
        )}</P>
        <CodeBlock code="COKAC_CODEX_PATH=/opt/codex/codex" />

        <h3 className="text-lg font-semibold text-white mt-6 mb-3"><IC>COKAC_AGY_PATH</IC></h3>
        <P>{t(
          <>Override the path to the Antigravity CLI <IC>agy</IC> binary. Same semantics as above but for Agy.</>,
          <>Antigravity CLI <IC>agy</IC> 바이너리 경로를 재지정합니다. 위와 동일한 의미이지만 Agy용입니다.</>
        )}</P>
        <CodeBlock code="COKAC_AGY_PATH=/usr/local/bin/agy" />

        <h3 className="text-lg font-semibold text-white mt-6 mb-3"><IC>COKAC_OPENCODE_PATH</IC></h3>
        <P>{t(
          <>Override the path to the Opencode CLI binary. Same semantics as above but for Opencode. <strong>Note:</strong> Opencode is not supported on Windows — setting this variable on Windows has no effect.</>,
          <>Opencode CLI 바이너리 경로를 재지정합니다. 위와 동일한 의미이지만 Opencode용입니다. <strong>참고:</strong> Opencode는 Windows에서 지원되지 않으므로, Windows에서 이 변수를 설정해도 효과가 없습니다.</>
        )}</P>
        <CodeBlock code="COKAC_OPENCODE_PATH=/usr/local/bin/opencode" />

        <h3 className="text-lg font-semibold text-white mt-6 mb-3"><IC>COKAC_FILE_ATTACH_THRESHOLD</IC></h3>
        <P>{t(
          <>Controls the size threshold (in bytes) at which the bot switches from sending a response as multiple chat messages to sending it as a single <IC>.txt</IC> file attachment. Applies to all chat platforms (Telegram, Discord, Slack).</>,
          <>봇이 응답을 여러 채팅 메시지로 나눠 보내는 대신 하나의 <IC>.txt</IC> 파일 첨부로 보내도록 전환하는 크기 임계값(바이트)을 제어합니다. 모든 채팅 플랫폼(Telegram, Discord, Slack)에 적용됩니다.</>
        )}</P>
        <UL>
          <li>{t(<><strong>Type:</strong> positive integer (bytes)</>, <><strong>타입:</strong> 양의 정수 (바이트)</>)}</li>
          <li>{t(
            <><strong>Default:</strong> <IC>8192</IC> (twice Telegram's 4096-byte per-message limit; the same default is used for Discord and Slack)</>,
            <><strong>기본값:</strong> <IC>8192</IC> (Telegram 메시지당 4096바이트 한계의 2배. Discord와 Slack에서도 동일한 기본값을 사용합니다)</>
          )}</li>
          <li>{t(
            <><strong>Behavior:</strong> Responses whose length exceeds this threshold are uploaded as a text file instead of being split into multiple chat messages. Lower the value if you prefer files sooner; raise it to keep more content inline.</>,
            <><strong>동작:</strong> 이 임계값을 초과하는 응답은 여러 채팅 메시지로 분할되는 대신 텍스트 파일로 업로드됩니다. 파일로 더 빨리 전환되길 원하면 값을 낮추고, 더 많은 내용을 인라인으로 유지하고 싶으면 값을 높이세요.</>
          )}</li>
          <li>{t(
            <><strong>Invalid values</strong> (non-numeric, negative, etc.) are silently ignored and the default is used.</>,
            <><strong>잘못된 값</strong>(숫자가 아니거나 음수 등)은 조용히 무시되고 기본값이 사용됩니다.</>
          )}</li>
        </UL>
        <CodeBlock code="COKAC_FILE_ATTACH_THRESHOLD=16384" />

        <h3 className="text-lg font-semibold text-white mt-6 mb-3"><IC>COKACDIR_DEBUG</IC></h3>
        <P>{t(
          <>Enable debug logging globally at startup. This is the programmatic way to turn on debug for automated runs and CI — achieving the same effect as manually toggling <IC>/debug</IC> to ON in every chat after the bot starts.</>,
          <>시작 시점부터 디버그 로깅을 전역적으로 활성화합니다. 자동화 실행이나 CI 환경에서 디버그를 프로그램적으로 켜는 방법이며, 봇이 시작된 후 모든 채팅에서 <IC>/debug</IC>를 수동으로 ON으로 토글하는 것과 동일한 효과를 냅니다.</>
        )}</P>
        <UL>
          <li>{t(
            <><strong>Type:</strong> string — set to exactly <IC>"1"</IC> to enable. The check is a strict string comparison (<IC>value == "1"</IC>), not a truthy coercion.</>,
            <><strong>타입:</strong> 문자열 — 활성화하려면 정확히 <IC>"1"</IC>로 설정합니다. 검사는 truthy coercion이 아니라 엄격한 문자열 비교(<IC>value == "1"</IC>)입니다.</>
          )}</li>
          <li>{t(<><strong>Default:</strong> not set.</>, <><strong>기본값:</strong> 설정되지 않음.</>)}</li>
          <li>{t(
            <><strong>Scope:</strong> global — affects all chats and all bots in the same process.</>,
            <><strong>범위:</strong> 전역 — 같은 프로세스의 모든 채팅과 모든 봇에 영향을 미칩니다.</>
          )}</li>
          <li>{t(
            <><strong>Behavior:</strong> When debug is ON, detailed logs for messenger API operations (Telegram, Discord, Slack), AI service calls, and the cron scheduler are printed to stdout. Once enabled at startup, you can still toggle it at runtime with <IC>/debug</IC>.</>,
            <><strong>동작:</strong> 디버그가 ON일 때, 메신저 API 작업(Telegram, Discord, Slack), AI 서비스 호출, 크론 스케줄러의 세부 로그가 stdout에 출력됩니다. 시작 시 활성화된 후에도 런타임에 <IC>/debug</IC>로 전환할 수 있습니다.</>
          )}</li>
        </UL>
        <CodeBlock code="COKACDIR_DEBUG=1" />
        <InfoBox type="warning">
          {t(
            <>
              <strong>This variable cannot disable debug on its own.</strong> The startup logic is a two-step check: (1) if <IC>COKACDIR_DEBUG</IC> equals <IC>"1"</IC>, debug is enabled immediately. (2) <strong>Otherwise</strong> — including when the variable is unset, empty, or set to any value other than <IC>"1"</IC> such as <IC>"0"</IC>, <IC>"false"</IC>, <IC>"true"</IC>, <IC>"yes"</IC> — cokacdir falls through to read <IC>~/.cokacdir/bot_settings.json</IC> and enables debug if <strong>any</strong> bot in that file has <IC>"debug": true</IC>. So setting <IC>COKACDIR_DEBUG=0</IC> does <em>not</em> guarantee debug is off; it only skips the env-var enable path. To definitively keep debug off, make sure no bot has <IC>"debug": true</IC> in <IC>bot_settings.json</IC> <strong>and</strong> that <IC>COKACDIR_DEBUG</IC> is not <IC>"1"</IC>. At runtime you can send <IC>/debug</IC> to flip the state back off, but note that <IC>/debug</IC> is a <strong>pure toggle</strong> — it takes no arguments and simply inverts the current state, so confirm the resulting state from the bot's reply.
            </>,
            <>
              <strong>이 변수 자체로는 디버그를 끌 수 없습니다.</strong> 시작 시 로직은 두 단계 검사입니다: (1) <IC>COKACDIR_DEBUG</IC>가 <IC>"1"</IC>과 같으면 즉시 디버그가 활성화됩니다. (2) <strong>그 외의 경우</strong> — 변수가 설정되지 않았거나, 비어 있거나, <IC>"0"</IC>, <IC>"false"</IC>, <IC>"true"</IC>, <IC>"yes"</IC>처럼 <IC>"1"</IC>이 아닌 값인 경우 — cokacdir는 <IC>~/.cokacdir/bot_settings.json</IC>을 읽는 단계로 폴백하고, 그 파일에 있는 <strong>어떤 봇이라도</strong> <IC>"debug": true</IC>로 설정되어 있으면 디버그를 활성화합니다. 따라서 <IC>COKACDIR_DEBUG=0</IC>으로 설정해도 디버그가 반드시 꺼지는 것은 <em>아닙니다</em> — 환경변수 enable 경로만 건너뛸 뿐입니다. 디버그를 확실히 끄려면 <IC>bot_settings.json</IC>의 어떤 봇에도 <IC>"debug": true</IC>가 없고 <strong>그리고</strong> <IC>COKACDIR_DEBUG</IC>가 <IC>"1"</IC>이 아닌 상태여야 합니다. 런타임에 끄고 싶으면 <IC>/debug</IC>를 보내 상태를 반전시킬 수 있지만, <IC>/debug</IC>는 인자를 받지 않고 현재 상태를 단순히 뒤집는 <strong>순수 토글</strong>이라는 점에 유의하세요 — 봇의 응답으로 결과 상태를 반드시 확인해야 합니다.
            </>
          )}
        </InfoBox>
      </SubSection>

      <SubSection title={String(t('/envvars — Inspect the Running Environment', '/envvars — 실행 중인 환경 확인'))}>
        <P>{t(
          <><IC>/envvars</IC> is a chat command that prints every environment variable currently visible to the bot process, along with its value. The variables are sorted alphabetically and rendered as <IC>KEY=VALUE</IC> pairs in the response.</>,
          <><IC>/envvars</IC>는 봇 프로세스에 현재 보이는 모든 환경변수와 그 값을 출력하는 채팅 명령입니다. 변수들은 알파벳순으로 정렬되어 <IC>KEY=VALUE</IC> 쌍으로 응답에 렌더링됩니다.</>
        )}</P>
        <CodeBlock code="/envvars" />

        <h3 className="text-lg font-semibold text-white mt-6 mb-3">
          {t('Access control', '접근 제어')}
        </h3>
        <UL>
          <li>{t(
            <><strong>Bot owner only.</strong> Non-owners are rejected with the message <IC>Only the bot owner can use /envvars.</IC> This matches the other admin-only commands in cokacdir.</>,
            <><strong>봇 소유자 전용입니다.</strong> 소유자가 아닌 경우 <IC>Only the bot owner can use /envvars.</IC> 메시지로 거부됩니다. 이것은 cokacdir의 다른 관리자 전용 명령과 동일한 방식입니다.</>
          )}</li>
          <li>{t(
            'The command is available only in a 1:1 chat with the bot. Group chats are rejected even for the owner, to avoid accidentally posting secrets where other members can read them.',
            '이 명령은 봇과의 1:1 채팅에서만 사용할 수 있습니다. 다른 멤버가 볼 수 있는 곳에 비밀값을 실수로 게시하지 않도록, 소유자라도 그룹 채팅에서는 거부됩니다.'
          )}</li>
        </UL>

        <InfoBox type="warning">
          {t(
            <>
              <strong>⚠ Security warning — /envvars exposes sensitive values.</strong> It dumps <strong>every</strong> environment variable visible to the bot process, including API keys, authentication tokens, database credentials, and anything else that happens to be exported. There is <strong>no redaction</strong> — the code comment in the implementation explicitly notes this is intentional for admin debugging on a personal, single-user bot.
            </>,
            <>
              <strong>⚠ 보안 경고 — /envvars는 민감한 값을 노출합니다.</strong> API 키, 인증 토큰, 데이터베이스 자격 증명, 그리고 export된 모든 값을 포함하여 봇 프로세스에 보이는 <strong>모든</strong> 환경변수를 덤프합니다. <strong>마스킹이 없습니다</strong> — 구현의 코드 주석은 이것이 개인/단일 사용자 봇의 관리자 디버깅을 위해 의도된 동작임을 명시합니다.
            </>
          )}
        </InfoBox>
        <P>{t(
          'Be aware of the following before using it:',
          '사용하기 전에 다음 사항을 숙지하세요:'
        )}</P>
        <UL>
          <li>{t(
            'Chat platforms (Telegram, Discord, Slack) all store message history on their servers. Anything you send via /envvars is persisted there until you delete the messages.',
            '채팅 플랫폼(Telegram, Discord, Slack)은 모두 메시지 기록을 자체 서버에 저장합니다. /envvars로 전송된 내용은 메시지를 삭제할 때까지 해당 서버에 남아 있습니다.'
          )}</li>
          <li>{t(
            'If you forward the response, screenshot it, or share your chat with anyone, the secrets are exposed.',
            '응답을 전달하거나 스크린샷을 찍거나 채팅을 누군가와 공유하면 비밀 정보가 노출됩니다.'
          )}</li>
          <li>{t(
            <>If a bot's owner account is ever compromised, the attacker can run <IC>/envvars</IC> and harvest every secret in your environment in one command.</>,
            <>봇 소유자 계정이 탈취되면, 공격자는 <IC>/envvars</IC>를 실행하여 환경의 모든 비밀을 한 번의 명령으로 수집할 수 있습니다.</>
          )}</li>
          <li>{t(
            <>Group chats are rejected for <IC>/envvars</IC>. Always use it in a 1:1 chat (DM) with the bot so secrets are not posted where other members can read them.</>,
            <>그룹 채팅에서는 <IC>/envvars</IC>가 거부됩니다. 비밀값이 다른 멤버가 읽을 수 있는 곳에 게시되지 않도록 항상 봇과의 1:1 채팅(DM)에서만 사용하세요.</>
          )}</li>
        </UL>
        <P>{t(
          <>Treat <IC>/envvars</IC> as a diagnostic tool for verifying configuration — for example, confirming that <IC>.env.json</IC> loaded correctly or that <IC>COKAC_CLAUDE_PATH</IC> is pointing where you expect — and clear the messages afterward.</>,
          <><IC>/envvars</IC>를 설정 검증을 위한 진단 도구로만 취급하세요 — 예를 들어 <IC>.env.json</IC>이 올바르게 로드되었는지, <IC>COKAC_CLAUDE_PATH</IC>가 예상한 위치를 가리키는지 확인하는 용도 — 그리고 사용 후에는 메시지를 삭제하세요.</>
        )}</P>

        <h3 className="text-lg font-semibold text-white mt-6 mb-3">
          {t('When to use it', '사용해야 할 때')}
        </h3>
        <UL>
          <li>{t(
            <>Verifying that <IC>~/.cokacdir/.env.json</IC> was loaded and your keys are applied.</>,
            <><IC>~/.cokacdir/.env.json</IC>이 로드되었고 키가 적용되었는지 확인할 때.</>
          )}</li>
          <li>{t(
            <>Checking whether a <IC>COKAC_*</IC> override is active in the running process.</>,
            <>실행 중인 프로세스에서 <IC>COKAC_*</IC> 재지정이 활성 상태인지 확인할 때.</>
          )}</li>
          <li>{t(
            'Diagnosing why a binary path override is not being picked up (for example, the variable is set but the file doesn\'t exist, so the fallback resolver ran instead).',
            '바이너리 경로 재지정이 적용되지 않는 이유를 진단할 때 (예: 변수는 설정되었지만 파일이 존재하지 않아 폴백 탐색기가 실행된 경우).'
          )}</li>
        </UL>
      </SubSection>

      <SubSection title={String(t('Troubleshooting', '문제 해결'))}>
        <UL>
          <li>{t(
            <><strong>My <IC>.env.json</IC> doesn't seem to load.</strong> Confirm the file is at exactly <IC>~/.cokacdir/.env.json</IC> (note the leading dot), that it is valid JSON, and that the root is a <strong>JSON object</strong> (<IC>{`{ ... }`}</IC>, not an array or a bare scalar). The values of that object's keys must each be a string, number, or boolean — objects, arrays, and <IC>null</IC> values are skipped with a warning. Run <IC>/envvars</IC> to see which variables are actually in the process environment.</>,
            <><strong><IC>.env.json</IC>이 로드되지 않는 것 같아요.</strong> 파일이 정확히 <IC>~/.cokacdir/.env.json</IC> 경로(앞의 점 주의)에 있는지, 유효한 JSON인지, 루트가 <strong>JSON 객체</strong>(<IC>{`{ ... }`}</IC>, 배열이나 단일 스칼라가 아님)인지 확인하세요. 그 객체의 각 키에 대한 값은 문자열, 숫자, 불리언 중 하나여야 합니다 — 객체, 배열, <IC>null</IC> 값은 경고와 함께 건너뜁니다. <IC>/envvars</IC>를 실행해서 실제로 프로세스 환경에 어떤 변수가 있는지 확인할 수 있습니다.</>
          )}</li>
          <li>{t(
            <><strong><IC>COKAC_CLAUDE_PATH</IC> is set but Claude still uses the wrong binary.</strong> The override is only used if the file at that path exists. If the path is wrong or the file is missing, cokacdir silently falls back to <IC>which claude</IC>. Double-check the path and file permissions.</>,
            <><strong><IC>COKAC_CLAUDE_PATH</IC>가 설정되었는데도 Claude가 잘못된 바이너리를 사용합니다.</strong> 해당 경로의 파일이 존재할 때만 재지정이 적용됩니다. 경로가 잘못되었거나 파일이 없으면, cokacdir는 조용히 <IC>which claude</IC>로 폴백합니다. 경로와 파일 권한을 다시 확인하세요.</>
          )}</li>
          <li>{t(
            <><strong><IC>/envvars</IC> returns "Only the bot owner can use /envvars."</strong> You are not registered as the owner of this bot. The owner is the Telegram user ID that first successfully interacted with the bot after it started; see the token management and first-chat guides for how ownership is established.</>,
            <><strong><IC>/envvars</IC>가 "Only the bot owner can use /envvars." 메시지를 반환합니다.</strong> 이 봇의 소유자로 등록되어 있지 않습니다. 소유자는 봇이 시작된 후 처음으로 성공적으로 상호작용한 Telegram 사용자 ID입니다. 소유권 설정 방법은 토큰 관리와 첫 번째 채팅 가이드를 참조하세요.</>
          )}</li>
        </UL>
      </SubSection>
    </div>
  )
}
