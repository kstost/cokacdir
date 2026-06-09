import { SectionTitle, SubSection, CodeBlock, StepList, StepItem, P, IC, InfoBox } from '../DocComponents'
import { useLanguage } from '../../LanguageContext'

export default function EC2Setup() {
  const { t } = useLanguage()
  return (
    <div>
      <SectionTitle>{t('Install on AWS EC2', 'AWS EC2에 설치하기')}</SectionTitle>
      <P>
        {t(
          'A guide to setting up a cokacdir & AI agent (Claude Code / Codex CLI / Gajae-Code) environment on AWS EC2 and using it anywhere via Telegram, Discord, or Slack.',
          'AWS EC2 위에 cokacdir & AI 에이전트 (Claude Code / Codex CLI / Gajae-Code) 환경을 만들고, 텔레그램, 디스코드 또는 Slack으로 어디서나 사용하는 가이드입니다.'
        )}
      </P>

      <InfoBox type="info">
        {t(
          <>You need 3 things: <strong>EC2 Public IPv4 address</strong>, <strong>PEM key file</strong> (for SSH), and a <strong>bot token</strong> for Telegram, Discord, or Slack.</>,
          <>준비물 3가지: <strong>EC2 퍼블릭 IPv4 주소</strong>, <strong>PEM 키 파일</strong> (SSH 접속용), 텔레그램, 디스코드 또는 Slack용 <strong>봇 토큰</strong>.</>
        )}
      </InfoBox>

      <SubSection title={String(t('Step 1. Create EC2 Instance', 'Step 1. EC2 인스턴스 만들기'))}>
        <P>
          {t(
            <>Go to <a href="https://ap-northeast-2.console.aws.amazon.com/ec2/home#LaunchInstances:" target="_blank" rel="noopener noreferrer" className="text-accent-cyan hover:underline">AWS EC2 Console → Launch Instances</a> and create a new instance.</>,
            <><a href="https://ap-northeast-2.console.aws.amazon.com/ec2/home#LaunchInstances:" target="_blank" rel="noopener noreferrer" className="text-accent-cyan hover:underline">AWS EC2 콘솔 → Launch Instances</a>에서 새 인스턴스를 만듭니다.</>
          )}
        </P>
        <StepList>
          <StepItem number={1} title={String(t('Set instance name', '인스턴스 이름 설정'))}>
            {t('Set the instance name.', '인스턴스 이름을 정합니다.')}
          </StepItem>
          <StepItem number={2} title={String(t('Select OS', 'OS 선택'))}>
            {t(<>Select <strong className="text-white">Ubuntu</strong> as the OS.</>, <>OS는 <strong className="text-white">Ubuntu</strong>를 선택합니다.</>)}
          </StepItem>
          <StepItem number={3} title={String(t('Create key pair', '키 페어 생성'))}>
            {t(
              <>Click <strong className="text-white">Create new key pair</strong> and download it as <IC>secret.pem</IC>.</>,
              <>키페어에서 <strong className="text-white">새 키 페어 생성</strong>을 눌러 <IC>secret.pem</IC> 이름으로 다운로드합니다.</>
            )}
          </StepItem>
          <StepItem number={4} title={String(t('Set storage & launch', '스토리지 설정 및 시작'))}>
            {t(
              <>Set the storage to <strong className="text-white">32 GB</strong>, then click <strong className="text-white">Launch instance</strong>.</>,
              <>스토리지 구성을 <strong className="text-white">32 GB</strong>로 설정한 뒤, <strong className="text-white">인스턴스 시작</strong> 버튼을 누릅니다.</>
            )}
          </StepItem>
        </StepList>

        <InfoBox type="tip">
          {t(
            <>Create a <strong>credential</strong> folder on your computer and place the <IC>secret.pem</IC> file inside.</>,
            <><IC>secret.pem</IC> 파일은 컴퓨터에 <strong>credential</strong> 폴더를 만들어 그 안에 넣어 둡니다.</>
          )}
        </InfoBox>

        <P>
          {t(
            <>Go to the <a href="https://ap-northeast-2.console.aws.amazon.com/ec2/home#Instances:" target="_blank" rel="noopener noreferrer" className="text-accent-cyan hover:underline">EC2 instance list</a>, click the instance you just created, and copy the <strong className="text-white">Public IPv4 address</strong> from the details.</>,
            <><a href="https://ap-northeast-2.console.aws.amazon.com/ec2/home#Instances:" target="_blank" rel="noopener noreferrer" className="text-accent-cyan hover:underline">EC2 인스턴스 목록</a>에서 방금 만든 인스턴스를 클릭하고, 세부정보의 <strong className="text-white">퍼블릭 IPv4 주소</strong>를 복사해 둡니다.</>
          )}
        </P>
      </SubSection>

      <SubSection title={String(t('Step 2. Create a Chat Bot', 'Step 2. 채팅 봇 만들기'))}>
        <P>
          {t(
            <>For Telegram, go to <a href="https://t.me/botfather" target="_blank" rel="noopener noreferrer" className="text-accent-cyan hover:underline">@BotFather</a> and press <strong className="text-white">START BOT</strong>. For Discord or Slack, follow the dedicated setup guide and keep the generated token ready.</>,
            <>텔레그램은 <a href="https://t.me/botfather" target="_blank" rel="noopener noreferrer" className="text-accent-cyan hover:underline">@BotFather</a>에서 <strong className="text-white">START BOT</strong>을 눌러 시작합니다. 디스코드 또는 Slack은 전용 설정 가이드를 따라 생성한 토큰을 준비하세요.</>
          )}
        </P>
        <StepList>
          <StepItem number={1}>
            {t(<>Type <IC>/newbot</IC>.</>, <><IC>/newbot</IC>을 입력합니다.</>)}
          </StepItem>
          <StepItem number={2}>
            {t(
              <>Set the bot's <strong className="text-white">name</strong> and <strong className="text-white">username</strong>. <span className="text-zinc-500">e.g. name: 'mybot', username: 'my_cokac_bot'</span></>,
              <>Bot의 <strong className="text-white">name</strong>과 <strong className="text-white">username</strong>을 정합니다. <span className="text-zinc-500">예: name은 '코깎봇', username은 'cokac_bot'</span></>
            )}
          </StepItem>
          <StepItem number={3}>
            {t('A token will be issued in the following format:', '토큰이 발급됩니다. 아래와 같은 형식입니다:')}
            <div className="mt-2">
              <IC>123456789:ABCdefGHIjklMNOpqrsTUVwxyz</IC>
            </div>
            <p className="text-zinc-500 text-sm mt-1">{t('Copy this token.', '이 토큰을 복사해 둡니다.')}</p>
          </StepItem>
        </StepList>
      </SubSection>

      <SubSection title={String(t('Step 3. Run EC2 Setup Command', 'Step 3. EC2 셋팅 명령어 실행'))}>
        <P>
          {t(
            <>Open a terminal in the <strong className="text-white">credential</strong> folder where <IC>secret.pem</IC> is located.</>,
            <><IC>secret.pem</IC> 파일이 들어 있는 <strong className="text-white">credential</strong> 폴더에서 터미널을 엽니다.</>
          )}
        </P>

        <div className="mt-6 mb-2 text-white font-semibold">macOS</div>
        <P>
          {t(
            <>Right-click the credential folder → <strong className="text-white">Services</strong> → <strong className="text-white">New Terminal at Folder</strong></>,
            <>credential 폴더를 우클릭 → <strong className="text-white">Services</strong> → <strong className="text-white">New Terminal at Folder</strong></>
          )}
        </P>
        <CodeBlock code={`export PEM=secret.pem
export IP=0.0.0.0
export URL=https://raw.githubusercontent.com/kstost/service-setup-cokacdir/refs/heads/main/basic_setup_ec2.sh
ssh -t -i "$PEM" ubuntu@$IP "bash -ic \\"source <(curl -sL $URL) > /dev/null 2>&1 && claude\\""
ssh -t -i "$PEM" ubuntu@$IP "bash -ic \\"curl -fsSL https://cokacdir.cokac.com/manage.sh | bash && cokacctl\\""` } />

        <div className="mt-6 mb-2 text-white font-semibold">Windows</div>
        <P>
          {t(
            <>Right-click the credential folder → <strong className="text-white">Open in Terminal</strong></>,
            <>credential 폴더를 우클릭 → <strong className="text-white">터미널에서 열기</strong></>
          )}
        </P>
        <CodeBlock code={`$PEM = "secret.pem"; \`
$IP = "0.0.0.0"; \`
$URL = "https://raw.githubusercontent.com/kstost/service-setup-cokacdir/refs/heads/main/basic_setup_ec2.sh"; \`
ssh -t -i $PEM ubuntu@$IP "bash -ic 'source <(curl -sL $URL) > /dev/null 2>&1 && claude'"
ssh -t -i $PEM ubuntu@$IP "bash -ic 'curl -fsSL https://cokacdir.cokac.com/manage.sh | bash && cokacctl'"` } />

        <InfoBox type="info">
          {t(
            'Replace the PEM file name and EC2 IP address with your own and run the command. First, the Claude Code initial setup screen will appear — complete the authentication. Then cokacctl will launch automatically for cokacdir setup.',
            '명령어 안의 PEM 파일 이름과 EC2 IP 주소를 본인 것으로 바꿔 넣고 실행하세요. 먼저 Claude Code 초기 설정 화면이 나타나면 인증을 완료합니다. 그 다음 cokacctl이 자동으로 실행되어 cokacdir 설정을 진행합니다.'
          )}
        </InfoBox>

        <StepList>
          <StepItem number={1} title={String(t('Claude Code authentication', 'Claude Code 인증'))}>
            {t(
              'The Claude Code setup screen appears first. Follow the prompts to complete authentication.',
              '먼저 Claude Code 설정 화면이 나타납니다. 안내에 따라 인증을 완료하세요.'
            )}
          </StepItem>
          <StepItem number={2} title={String(t('cokacctl dashboard', 'cokacctl 대시보드'))}>
            {t(
              <>After authentication, the cokacctl dashboard (for managing cokacdir) appears automatically. Press <IC>k</IC> to register your Telegram, Discord, or Slack token, then press <IC>s</IC> to start the server.</>,
              <>인증이 끝나면 cokacctl 대시보드(cokacdir 관리 화면)가 자동으로 나타납니다. <IC>k</IC>를 눌러 텔레그램, 디스코드 또는 Slack 토큰을 등록한 후, <IC>s</IC>를 눌러 서버를 시작합니다.</>
            )}
          </StepItem>
          <StepItem number={3} title={String(t('Done', '완료'))}>
            {t(
              <>Once the server is running, press <IC>q</IC> to exit the dashboard and close the terminal window. Then go to <IC>https://t.me/[your bot username]</IC> and press <strong className="text-white">START BOT</strong> to start chatting with your AI agent.</>,
              <>서버가 구동되면 <IC>q</IC>를 눌러 대시보드를 빠져나가고 터미널 창을 닫으면 됩니다. 이후 <IC>https://t.me/[앞서 정한 username]</IC>으로 접속해서 <strong className="text-white">START BOT</strong>을 누르면 AI 에이전트와 대화할 수 있습니다.</>
            )}
          </StepItem>
        </StepList>
      </SubSection>

      <SubSection title={String(t('EC2 Dashboard (cokacctl)', 'EC2 대시보드 (cokacctl)'))}>
        <P>
          {t(
            'You can manage everything from the cokacctl dashboard — updates, token management, server start/stop, and more. Connect to your EC2 and run cokacctl with the following command.',
            'cokacctl 대시보드에서 업데이트, 토큰 관리, 서버 시작/종료 등 모든 것을 관리할 수 있습니다. 다음 명령어로 EC2에 접속하여 cokacctl을 실행하세요.'
          )}
        </P>

        <div className="mt-4 mb-2 text-white font-semibold">macOS</div>
        <CodeBlock code={`export PEM=secret.pem
export IP=0.0.0.0
ssh -t -i "$PEM" ubuntu@$IP "bash -ic \\"cokacctl\\""` } />

        <div className="mt-4 mb-2 text-white font-semibold">Windows</div>
        <CodeBlock code={`$PEM = "secret.pem"; \`
$IP = "0.0.0.0"; \`
ssh -t -i $PEM ubuntu@$IP "bash -ic 'cokacctl'"` } />

        <InfoBox type="info">
          {t(
            'Replace the PEM file name and EC2 IP address with your own and run the command.',
            '명령어 안의 PEM 파일 이름과 EC2 IP 주소를 본인 것으로 바꿔 넣고 실행하세요.'
          )}
        </InfoBox>
      </SubSection>
    </div>
  )
}
