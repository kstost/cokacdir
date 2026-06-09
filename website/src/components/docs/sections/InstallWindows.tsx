import { SectionTitle, SubSection, StepList, StepItem, P, CodeBlock, IC, InfoBox } from '../DocComponents'
import { useLanguage } from '../../LanguageContext'

export default function InstallWindows() {
  const { t } = useLanguage()
  return (
    <div>
      <SectionTitle>{t('Install on Windows', 'Windows에 설치하기')}</SectionTitle>
      <P>
        {t(
          'Install cokacdir on a Windows PC and use AI coding agents via Telegram, Discord, or Slack bot.',
          'Windows PC에 cokacdir을 설치하고 텔레그램, 디스코드 또는 Slack 봇으로 AI 코딩 에이전트를 사용하세요.'
        )}
      </P>

      <SubSection title={String(t('Step 1. Install AI Agent', 'Step 1. AI 에이전트 설치'))}>
        <P>
          {t(
            <>cokacdir requires at least one supported AI agent, such as <strong className="text-white">Claude Code</strong>, <strong className="text-white">Codex CLI</strong>, or <strong className="text-white">Gajae-Code</strong>.</>,
            <>cokacdir은 <strong className="text-white">Claude Code</strong>, <strong className="text-white">Codex CLI</strong>, <strong className="text-white">Gajae-Code</strong> 같은 지원 AI 에이전트가 하나 이상 필요합니다.</>
          )}
        </P>

        <div className="mt-4 mb-2 text-white font-semibold">{t('Option A: Claude Code', '옵션 A: Claude Code')}</div>
        <StepList>
          <StepItem number={1} title={String(t('Install Git', 'Git 설치'))}>
            {t(
              <>Download and install Git from <a href="https://git-scm.com/downloads/win" target="_blank" rel="noopener noreferrer" className="text-accent-cyan hover:underline">git-scm.com/downloads/win</a>.</>,
              <><a href="https://git-scm.com/downloads/win" target="_blank" rel="noopener noreferrer" className="text-accent-cyan hover:underline">git-scm.com/downloads/win</a>에서 Git을 다운로드하여 설치하세요.</>
            )}
          </StepItem>
          <StepItem number={2} title={String(t('Install Claude Code', 'Claude Code 설치'))}>
            {t('Open PowerShell and run:', 'PowerShell을 열고 실행하세요:')}
            <CodeBlock code="irm https://claude.ai/install.ps1 | iex" />
          </StepItem>
          <StepItem number={3} title={String(t('Update PATH', 'PATH 업데이트'))}>
            {t('Run this command immediately after installation:', '설치 직후 이 명령어를 실행하세요:')}
            <CodeBlock code={`[Environment]::SetEnvironmentVariable("Path", [Environment]::GetEnvironmentVariable("Path", "User") + ";$env:USERPROFILE\\.local\\bin", "User")`} />
          </StepItem>
          <StepItem number={4} title={String(t('Initial setup', '초기 설정'))}>
            {t(<><strong className="text-white">Open a new PowerShell tab</strong> and run:</>, <><strong className="text-white">새 PowerShell 탭을 열고</strong> 실행하세요:</>)}
            <CodeBlock code="claude" />
            {t('Follow the prompts for initial setup and login.', '안내에 따라 초기 설정과 로그인을 완료하세요.')}
          </StepItem>
        </StepList>
        <InfoBox type="warning">
          {t(
            'You must open a new PowerShell tab after step 3 for the PATH changes to take effect.',
            '3단계 후 반드시 새 PowerShell 탭을 열어야 PATH 변경이 적용됩니다.'
          )}
        </InfoBox>

        <div className="mt-8 mb-2 text-white font-semibold">{t('Option B: Codex CLI', '옵션 B: Codex CLI')}</div>
        <StepList>
          <StepItem number={1} title={String(t('Install Node.js', 'Node.js 설치'))}>
            {t(
              <>Download and install Node.js from <a href="https://nodejs.org/ko/download" target="_blank" rel="noopener noreferrer" className="text-accent-cyan hover:underline">nodejs.org/download</a>.</>,
              <><a href="https://nodejs.org/ko/download" target="_blank" rel="noopener noreferrer" className="text-accent-cyan hover:underline">nodejs.org/download</a>에서 Node.js를 다운로드하여 설치하세요.</>
            )}
          </StepItem>
          <StepItem number={2} title={String(t('Set execution policy', '실행 정책 설정'))}>
            {t('Open PowerShell and run:', 'PowerShell을 열고 실행하세요:')}
            <CodeBlock code="Set-ExecutionPolicy -Scope CurrentUser -ExecutionPolicy RemoteSigned" />
          </StepItem>
          <StepItem number={3} title={String(t('Install Codex', 'Codex 설치'))}>
            <CodeBlock code="npm i -g @openai/codex" />
          </StepItem>
          <StepItem number={4} title={String(t('Login', '로그인'))}>
            {t('Log in to your OpenAI account to complete the setup.', 'OpenAI 계정에 로그인하여 설정을 완료하세요.')}
          </StepItem>
        </StepList>

        <div className="mt-8 mb-2 text-white font-semibold">{t('Option C: Gajae-Code', '옵션 C: Gajae-Code')}</div>
        <StepList>
          <StepItem number={1} title={String(t('Install Bun', 'Bun 설치'))}>
            {t('Install Bun from bun.sh, then open a new terminal.', 'bun.sh에서 Bun을 설치한 뒤 새 터미널을 여세요.')}
          </StepItem>
          <StepItem number={2} title={String(t('Install Gajae-Code', 'Gajae-Code 설치'))}>
            <CodeBlock code="bun install -g gajae-code" />
          </StepItem>
          <StepItem number={3} title={String(t('Run Gajae-Code', 'Gajae-Code 실행'))}>
            <CodeBlock code="gjc" />
          </StepItem>
        </StepList>
      </SubSection>

      <SubSection title={String(t('Step 2. Install cokacdir', 'Step 2. cokacdir 설치'))}>
        <P>{t('Open PowerShell as Administrator and run:', '관리자 권한 PowerShell을 열고 실행하세요:')}</P>
        <CodeBlock code="irm https://cokacdir.cokac.com/manage.ps1 | iex; cokacctl" />
      </SubSection>

      <SubSection title={String(t('Step 3. Initial Setup with cokacctl', 'Step 3. cokacctl로 초기 설정'))}>
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
