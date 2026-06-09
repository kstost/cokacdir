import { SectionTitle, SubSection, StepList, StepItem, P, CodeBlock, IC, InfoBox } from '../DocComponents'
import { useLanguage } from '../../LanguageContext'

export default function InstallMacOS() {
  const { t } = useLanguage()
  return (
    <div>
      <SectionTitle>{t('Install on macOS', 'macOS에 설치하기')}</SectionTitle>
      <P>
        {t(
          'Install cokacdir on macOS and use AI coding agents via Telegram, Discord, or Slack bot.',
          'macOS에 cokacdir을 설치하고 텔레그램, 디스코드 또는 Slack 봇으로 AI 코딩 에이전트를 사용하세요.'
        )}
      </P>

      <SubSection title={String(t('Before You Begin: Install AI Agent', '시작 전: AI 에이전트 설치'))}>
        <P>
          {t(
            <>cokacdir requires at least one supported AI agent, such as <strong className="text-white">Claude Code</strong>, <strong className="text-white">Codex CLI</strong>, or <strong className="text-white">Gajae-Code</strong>.</>,
            <>cokacdir은 <strong className="text-white">Claude Code</strong>, <strong className="text-white">Codex CLI</strong>, <strong className="text-white">Gajae-Code</strong> 같은 지원 AI 에이전트가 하나 이상 필요합니다.</>
          )}
        </P>
        <div className="mt-4 mb-2 text-white font-semibold">Claude Code</div>
        <CodeBlock code="curl -fsSL https://claude.ai/install.sh | bash" />
        <div className="mt-4 mb-2 text-white font-semibold">Codex CLI</div>
        <CodeBlock code="npm i -g @openai/codex" />
        <div className="mt-4 mb-2 text-white font-semibold">Gajae-Code</div>
        <CodeBlock code="bun install -g gajae-code" />
      </SubSection>

      <SubSection title={String(t('Step 1. Install cokacdir', 'Step 1. cokacdir 설치'))}>
        <P>{t('Open a terminal and run:', '터미널을 열고 실행하세요:')}</P>
        <CodeBlock code="curl -fsSL https://cokacdir.cokac.com/manage.sh | bash && cokacctl" />
      </SubSection>

      <SubSection title={String(t('Step 2. Initial Setup with cokacctl', 'Step 2. cokacctl로 초기 설정'))}>
        <StepList>
          <StepItem number={1} title={String(t('Install cokacdir', 'cokacdir 설치'))}>
            {t(<>Press <IC>i</IC> to install cokacdir.</>, <><IC>i</IC>를 눌러 cokacdir를 설치합니다.</>)}
          </StepItem>
          <StepItem number={2} title={String(t('Register bot token', '봇 토큰 등록'))}>
            {t(
              <>Press <IC>k</IC> to open the token input screen, paste your bot token, then press Enter.</>,
              <><IC>k</IC>를 눌러 토큰 입력 화면을 열고, 봇 토큰을 붙여넣은 후 Enter를 누릅니다.</>
            )}
          </StepItem>
          <StepItem number={3} title={String(t('Start the server', '서버 시작'))}>
            {t(<>Press <IC>s</IC> to start the server.</>, <><IC>s</IC>를 눌러 서버를 시작합니다.</>)}
          </StepItem>
        </StepList>
        <InfoBox type="tip">
          {t(
            <>You need a bot token from Telegram, Discord, or Slack. See the setup guides and Token Management for details.</>,
            <>텔레그램, 디스코드 또는 Slack 봇 토큰이 필요합니다. 자세한 내용은 설정 가이드와 토큰 관리를 참고하세요.</>
          )}
        </InfoBox>
      </SubSection>

      <SubSection title={String(t('Server Controls (cokacctl)', '서버 제어 (cokacctl)'))}>
        <P>{t(<>Run <IC>cokacctl</IC> anytime to manage the server:</>, <>언제든 <IC>cokacctl</IC>을 실행하여 서버를 관리합니다:</>)}</P>
        <StepList>
          <StepItem number={1}>
            <IC>s</IC> — {t('Start server as background service (persists after reboot)', '백그라운드 서비스로 서버 시작 (재부팅 후에도 유지)')}
          </StepItem>
          <StepItem number={2}>
            <IC>t</IC> — {t('Stop server (restarts on reboot)', '서버 중지 (재부팅 시 다시 시작)')}
          </StepItem>
          <StepItem number={3}>
            <IC>r</IC> — {t('Restart the server', '서버 재시작')}
          </StepItem>
          <StepItem number={4}>
            <IC>d</IC> — {t('Stop and remove background registration', '중지 및 백그라운드 등록 해제 (재부팅 시 시작 안 됨)')}
          </StepItem>
        </StepList>
      </SubSection>
    </div>
  )
}
