import { Link } from 'react-router-dom'
import {
  MessageSquare,
  FolderOpen,
  Upload,
  Terminal,
  Users,
  Clock,
  Wrench,
  FileText,
  ArrowRight,
  Bot,
} from 'lucide-react'
import Footer from '../components/Footer'
import { useLanguage, LangToggle } from '../components/LanguageContext'

export default function LandingPage() {
  const { t } = useLanguage()

  const features = [
    {
      icon: <MessageSquare size={24} />,
      title: t('Telegram, Discord & Slack', '텔레그램, 디스코드 & Slack'),
      desc: t(
        'Chat with AI coding agents from your phone or desktop. Create a bot and start coding from anywhere.',
        '스마트폰이나 데스크톱에서 AI 코딩 에이전트와 채팅하세요. 봇을 만들고 어디서나 코딩을 시작하세요.'
      ),
      color: 'text-accent-cyan',
      bg: 'bg-accent-cyan/10',
    },
    {
      icon: <FolderOpen size={24} />,
      title: t('Session Management', '세션 관리'),
      desc: t(
        'Create, resume, and switch between workspaces. Sessions persist across restarts with auto-restore.',
        '워크스페이스를 생성, 재개, 전환하세요. 세션은 재시작 시 자동 복원됩니다.'
      ),
      color: 'text-accent-purple',
      bg: 'bg-accent-purple/10',
    },
    {
      icon: <Upload size={24} />,
      title: t('File Transfer', '파일 전송'),
      desc: t(
        'Upload photos, documents, videos, and more directly to your workspace. Download files with a single command.',
        '사진, 문서, 동영상 등을 워크스페이스에 직접 업로드하세요. 명령 하나로 파일을 다운로드하세요.'
      ),
      color: 'text-accent-green',
      bg: 'bg-accent-green/10',
    },
    {
      icon: <Terminal size={24} />,
      title: t('Shell Commands', '셸 명령어'),
      desc: t(
        'Execute shell commands directly with the ! prefix. Real-time output streaming with process control.',
        '! 접두사로 셸 명령어를 직접 실행하세요. 실시간 출력 스트리밍과 프로세스 제어를 지원합니다.'
      ),
      color: 'text-yellow-400',
      bg: 'bg-yellow-400/10',
    },
    {
      icon: <Users size={24} />,
      title: t('Multi-Bot Collaboration', '멀티봇 협업'),
      desc: t(
        'Run multiple AI bots in group chats. Shared context, targeted messages, and coordinated workflows.',
        '그룹 채팅에서 여러 AI 봇을 실행하세요. 공유 컨텍스트, 지정 메시지, 협업 워크플로를 지원합니다.'
      ),
      color: 'text-pink-400',
      bg: 'bg-pink-400/10',
    },
    {
      icon: <Clock size={24} />,
      title: t('Task Scheduling', '예약 작업'),
      desc: t(
        'Schedule tasks with natural language. One-time or recurring jobs with isolated workspaces.',
        '자연어로 작업을 예약하세요. 일회성 또는 반복 작업을 독립된 워크스페이스에서 실행합니다.'
      ),
      color: 'text-orange-400',
      bg: 'bg-orange-400/10',
    },
    {
      icon: <Wrench size={24} />,
      title: t('Tool Management', '도구 관리'),
      desc: t(
        'Fine-grained control over which tools the AI can use per chat. Enable or disable tools on the fly.',
        '채팅별로 AI가 사용할 수 있는 도구를 세밀하게 제어하세요. 도구를 즉시 활성화/비활성화할 수 있습니다.'
      ),
      color: 'text-primary-light',
      bg: 'bg-primary-light/10',
    },
    {
      icon: <FileText size={24} />,
      title: t('Custom Instructions', '커스텀 지시사항'),
      desc: t(
        'Set persistent per-chat instructions. Guide AI behavior with custom prompts that survive restarts.',
        '채팅별 영구 지시사항을 설정하세요. 재시작 후에도 유지되는 커스텀 프롬프트로 AI 동작을 안내합니다.'
      ),
      color: 'text-teal-400',
      bg: 'bg-teal-400/10',
    },
  ]

  return (
    <div className="min-h-screen bg-bg-dark">
      {/* Hero */}
      <section className="relative min-h-screen flex items-center justify-center overflow-hidden grid-background">
        {/* Lang toggle */}
        <div className="absolute top-5 right-6 z-20">
          <LangToggle />
        </div>

        {/* Gradient orbs */}
        <div className="absolute top-1/4 left-1/4 w-96 h-96 bg-accent-cyan/10 rounded-full blur-3xl animate-glow-pulse" />
        <div className="absolute bottom-1/4 right-1/4 w-96 h-96 bg-accent-purple/10 rounded-full blur-3xl animate-glow-pulse" style={{ animationDelay: '2s' }} />

        <div className="relative z-10 text-center px-6 max-w-4xl mx-auto">
          <div>
            <div className="flex items-center justify-center gap-3 mb-6">
              <Bot size={40} className="text-accent-cyan" />
            </div>
            <h1 className="text-5xl md:text-7xl font-extrabold mb-4 tracking-tight">
              <span className="gradient-text">cokacdir</span>
            </h1>
            <p className="text-xl md:text-2xl text-zinc-300 mb-4 font-medium">
              {t('AI Coding Agents, Anywhere', 'AI 코딩 에이전트, 어디서나')}
            </p>
            <p className="text-zinc-500 text-lg max-w-2xl mx-auto mb-10 leading-relaxed">
              {t(
        'Run AI coding agents like Claude Code, Codex, and Gajae-Code through Telegram, Discord, and Slack bots. Code from your phone, tablet, or any device with a chat app.',
                'Claude Code, Codex, Gajae-Code 등 AI 코딩 에이전트를 텔레그램, 디스코드, Slack 봇으로 실행하세요. 스마트폰, 태블릿, 채팅 앱이 있는 어떤 기기에서든 코딩하세요.'
              )}
            </p>
          </div>

          <div className="flex flex-col sm:flex-row items-center justify-center gap-4 mb-8">
            <Link
              to="/docs/install-windows"
              className="px-8 py-3 bg-accent-cyan text-bg-dark font-semibold rounded-lg hover:bg-accent-cyan/90 transition-colors glow-cyan flex items-center gap-2"
            >
              {t('Get Started', '시작하기')} <ArrowRight size={18} />
            </Link>
            <Link
              to="/docs"
              className="px-8 py-3 border border-zinc-700 text-zinc-300 font-medium rounded-lg hover:border-zinc-500 hover:text-white transition-colors"
            >
              {t('Documentation', '문서')}
            </Link>
          </div>

          {/* Install Command */}
          <div className="mt-6 text-left max-w-xl mx-auto">
            <p className="text-zinc-400 text-sm font-semibold mb-3">{t('Install Command', '설치 명령어')}</p>
            <div className="mb-3">
              <p className="text-zinc-500 text-xs mb-1.5">macOS / Linux:</p>
              <code className="text-sm font-mono text-accent-cyan bg-bg-card border border-zinc-800 px-4 py-2 rounded-lg block overflow-x-auto">
                curl -fsSL https://cokacdir.cokac.com/manage.sh | bash && cokacctl
              </code>
            </div>
            <div>
              <p className="text-zinc-500 text-xs mb-1.5">{t('Windows (PowerShell as Administrator):', 'Windows (관리자 권한 PowerShell):')}</p>
              <code className="text-sm font-mono text-accent-cyan bg-bg-card border border-zinc-800 px-4 py-2 rounded-lg block overflow-x-auto">
                irm https://cokacdir.cokac.com/manage.ps1 | iex; cokacctl
              </code>
            </div>
          </div>
        </div>
      </section>

      {/* Features */}
      <section className="py-24 px-6">
        <div className="max-w-6xl mx-auto">
          <div className="text-center mb-16">
            <h2 className="text-3xl md:text-4xl font-bold text-white mb-4">
              {t('Everything you need', '필요한 모든 것')}
            </h2>
            <p className="text-zinc-500 text-lg max-w-xl mx-auto">
              {t(
                'A complete platform for running AI coding agents through chat interfaces',
                '채팅 인터페이스로 AI 코딩 에이전트를 실행하는 완전한 플랫폼'
              )}
            </p>
          </div>

          <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-4 gap-5">
            {features.map((f) => (
              <div
                key={String(f.title)}
                className="bg-bg-card border border-zinc-800/50 rounded-xl p-6 hover:border-zinc-700 transition-colors"
              >
                <div className={`${f.bg} ${f.color} w-10 h-10 rounded-lg flex items-center justify-center mb-4`}>
                  {f.icon}
                </div>
                <h3 className="text-white font-semibold mb-2">{f.title}</h3>
                <p className="text-zinc-500 text-sm leading-relaxed">{f.desc}</p>
              </div>
            ))}
          </div>
        </div>
      </section>

      {/* Quick Start */}
      <section className="py-24 px-6 bg-bg-card/50">
        <div className="max-w-3xl mx-auto">
          <div className="text-center mb-12">
            <h2 className="text-3xl font-bold text-white mb-4">
              {t('Get up and running in 3 steps', '3단계로 시작하기')}
            </h2>
          </div>

          <div className="space-y-8">
            {[
              {
                step: 1,
                title: t('Install cokacdir', 'cokacdir 설치'),
                desc: (
                  <div className="mt-2">
                    <p className="text-zinc-500 text-xs mb-1.5">macOS / Linux:</p>
                    <code className="text-sm font-mono text-accent-cyan bg-bg-dark border border-zinc-800 px-3 py-1.5 rounded block overflow-x-auto">
                      curl -fsSL https://cokacdir.cokac.com/manage.sh | bash && cokacctl
                    </code>
                    <p className="text-zinc-500 text-xs mb-1.5 mt-3">{t('Windows (PowerShell as Administrator):', 'Windows (관리자 권한 PowerShell):')}</p>
                    <code className="text-sm font-mono text-accent-cyan bg-bg-dark border border-zinc-800 px-3 py-1.5 rounded block overflow-x-auto">
                      irm https://cokacdir.cokac.com/manage.ps1 | iex; cokacctl
                    </code>
                  </div>
                ),
              },
              {
                step: 2,
                title: t('Create a bot & register token', '봇 생성 및 토큰 등록'),
                desc: (
                  <p className="text-zinc-400">
                    {t(
                      <>
                        Create a Telegram bot via{' '}
                        <span className="text-accent-cyan">@BotFather</span>, a Discord bot at the Developer Portal, or a Slack app with Socket Mode.
                        Press <code className="font-mono text-accent-cyan bg-bg-dark px-1.5 py-0.5 rounded text-sm">k</code> in
                        cokacctl to register your token.
                      </>,
                      <>
                        텔레그램에서 <span className="text-accent-cyan">@BotFather</span>를 통해 봇을 만들거나
                        디스코드 개발자 포털에서 봇을 생성하고, Slack은 Socket Mode 앱을 생성하세요.
                        cokacctl에서 <code className="font-mono text-accent-cyan bg-bg-dark px-1.5 py-0.5 rounded text-sm">k</code>를
                        눌러 토큰을 등록하세요.
                      </>
                    )}
                  </p>
                ),
              },
              {
                step: 3,
                title: t('Start the server', '서버 시작'),
                desc: (
                  <p className="text-zinc-400">
                    {t(
                      <>
                        Press <code className="font-mono text-accent-cyan bg-bg-dark px-1.5 py-0.5 rounded text-sm">s</code> in
                        cokacctl. Open your chat app and start coding with AI.
                      </>,
                      <>
                        cokacctl에서 <code className="font-mono text-accent-cyan bg-bg-dark px-1.5 py-0.5 rounded text-sm">s</code>를
                        누르세요. 채팅 앱을 열고 AI와 코딩을 시작하세요.
                      </>
                    )}
                  </p>
                ),
              },
            ].map((item) => (
              <div key={item.step} className="flex gap-5">
                <div className="flex-shrink-0 w-10 h-10 rounded-full bg-accent-cyan/20 text-accent-cyan flex items-center justify-center font-bold text-lg">
                  {item.step}
                </div>
                <div className="flex-1 pt-1">
                  <h3 className="text-white font-semibold text-lg mb-2">{item.title}</h3>
                  {item.desc}
                </div>
              </div>
            ))}
          </div>

          <div className="text-center mt-12">
            <Link
              to="/docs/install-windows"
              className="inline-flex items-center gap-2 text-accent-cyan hover:text-accent-cyan/80 font-medium transition-colors"
            >
              {t('Read the full installation guide', '설치 가이드 전체 보기')} <ArrowRight size={16} />
            </Link>
          </div>
        </div>
      </section>

      <Footer />
    </div>
  )
}
